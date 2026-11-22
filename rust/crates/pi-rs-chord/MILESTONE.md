# Milestone 2.1a foundation checkpoint

Base commit: `780bf31fc`. Updated 2026-11-22 UTC. No commits created.
**Slice 3 implementation and bounded functional gates: COMPLETE.**
The foundation is ready for coordinator review and the additive telemetry bridge.
Full upstream behavior/resource equivalence is NOT established by this checkpoint.
All authored changes stayed inside `rust/crates/pi-rs-chord/`. Sibling crates,
`packages/`, `apps/`, shared tracking and governing instructions were not edited.

## Acceptance gates

| Gate | Result | Evidence |
| --- | --- | --- |
| Formatting | PASS | `cargo fmt --check`, final unit `chord-s3-final` |
| Warning-free lint | PASS | `cargo clippy --offline -j 2 --all-targets -- -D warnings` |
| Complete offline Rust suite | PASS | Unified workspace `cargo test -p pi-rs-chord`: 107/107, zero ignored (8.48s) |
| Store/schema/archive | PASS, bounded | 7 store, 2 evolution, 3 read-plan/live and 1 document integration tests |
| Async sync/history/scheduler | PASS, bounded | 5 client/history and 2 deterministic scheduler tests |
| Link admission/runtime | PASS, bounded | 2 shape and 5 runtime tests plus 4 existing binary/parts tests |
| Untouched TS wire interop | PASS, bounded | All 19 binary AND segmented-text tag fixtures match byte-for-byte |
| Telemetry lifecycle | PASS, bounded | 7 runtime tests, 3 storage/ingest tests, plus `tests/telemetry_bridge.rs` |
| Earlier foundation regressions | PASS | All 72 Slice 2 tests retained and passing |
| New-test sensitivity | PASS | All 34 Slice 3 tests + `telemetry_bridge` failed on covered mutations/bugs |
| Authored Rust safety | PASS | `#![forbid(unsafe_code)]`, no `unwrap()`, `expect()` or `panic!` in `src/` |
| Temporary cleanup | PASS | No `.log` files, oracle recorder and scratch test draft removed |
| Complete TS/resource parity | NOT PROVEN | See fidelity limits below |
| Shared telemetry bridge | PASS | Landed in `pi-rs-telemetry` (`TelemetryRecorderSink`, `FallibleTelemetryStore`) and `SqliteTelemetry::telemetry_context()`; verified in `tests/telemetry_bridge.rs` (error propagation mutation-checked) |

Final evidence: `target/slice3-final-{fmt,lint,tests}.txt` (ignored build artifacts).
Slice 3 tests added: 34. Tests removed: 0. One new test was renamed for accuracy.
Before: 72 tests, 2.21s summed execution. After: 106 tests, 12.02s summed execution.
Final compilation took 6m02s, lint 56.16s. Test profile used
`CARGO_PROFILE_TEST_DEBUG=0` to reduce shared-host linker I/O, not a manifest change.
The suite now includes 9 MiB upload data and a deliberate 2s backpressure observation.
These runtime totals are not a performance comparison under shared-host contention.

## Slice 3 implementation

- `store::{table,writes,applier,expression,query}`: owner-qualified STRICT tables,
  WITHOUT ROWID except integer primary keys, safe-integer/boolean SQL CHECKs,
  closed typed writes, JSON value/version pairs, stable cursors and SQL null semantics.
  Replay validates affected counts, rolls back divergence and refuses further replay.
- `store::{claims,evolution,json_column}`: identity-based ownership and replay exclusion,
  validated histories, pre/data/post evolution phases, resumable 100-row migrations,
  empty updates that still stamp row versions, lazy cached JSON migration validation.
  JSON values are not walked on current-version writes or eagerly rewritten on access.
- `store::{read_plan,live_query}`: checked bindings, EXISTS and joined projections,
  ranges/counts/probes, table dependency metadata, commit-framed publication,
  unchanged-row revision reuse, generation-checked rebind/close and fallible decoding.
  User decoders run outside the query mutex and stale callback results are discarded.
- `store::{step,archive}` and `documents`: document retirement/creation/change phases,
  staged SQL, one outbox sequence, SQL rollback before in-memory adoption, shared
  batch publication. Completion callbacks finish before releasing the settlement line.
  Format-1 gzip JSONL export/import preserves stored versions, bases/deltas and indexes.
  Import is transactional and rejects bad end counts, document lifetimes and ordering.
  Streaming export holds the SQLite queue with two 64 KiB chunks of read-ahead,
  and dropping its unread body releases the queue. Reader import avoids whole-body buffering.
- `sync::{client,scheduler}`: asynchronous remote/state hooks, filesystem adapter,
  restore/history selection, inside-batch resume, acknowledgement-before-trim,
  unsynced-local refusal, archive restore into an empty database, truncated gzip retry.
  Upload batches stop at 8 MiB, pull adoption at 4 MiB. One oversized commit is
  indivisible, as upstream. Outbox pages start at 100 rows and adapt toward 1 MiB
  from observed average size, also as upstream. Earliest urgency survives running
  flushes, retries back off, close cancels timers and performs a final fallible flush.
- `link::{text,transport,channel}` and binary admission: all 19 positional layouts,
  closed error envelopes, principal kind admission, origin/trace and raw byte segments,
  hello-before-queue, serialized asynchronous handlers, pong fences, pause-aware
  heartbeat/timeouts, FIFO memory links, IPC callback/backpressure semantics,
  binary-only WebSocket framing with payload/queue/buffer bounds and close codes.
- `telemetry::{recorder,facet,sender}`: parent/carrier context, explicit status,
  events/links, inert settled spans, measurement intervals, cached page/worker schemas,
  registry hooks, principal admission/stamping and existing per-page budgets.
  Unbound sends requeue, refused accepted sends are best effort. Cancellation releases
  capacity. Single weak timers stop cleanly, SQLite flush owns admitted work after
  caller cancellation, close flushes partial metrics and preserves database ownership.

## Verification and fixes found

The complete suite found a real document settlement race: the line was released
before staged completion callbacks ran. Moving release after callbacks fixed the
existing dropped-caller/fence regression. A bounded decoder regression exposed a
query-mutex lock inversion and now proves reentrant close cannot publish stale data.
Other pre-fix failures caught wire-shape admission, inert recording identity,
reserved sender capacity, empty migration stamps and tagged-enum JSON decoding.

`target/slice3-mutation.txt` records 27 behavior failures, including the strengthened
existing binary oracle test. Two initially surviving tests were strengthened, then
failed in `slice3-mutation-stream.txt` and `slice3-mutation-owned-flush.txt`.
The remaining new tests have pre-fix failures in `slice3-link-red.txt`,
`slice3-telemetry-red.txt`, `slice3-evolution-red2.txt`, `slice3-read-tests.txt`
and `slice3-reentrant-red.txt`. All source mutations were restored byte-for-byte,
verified against `slice3-pre-mutation-sha256.txt`. Only the two deliberately
strengthened test files differ. `slice3-final-sha256.txt` records the final tree.
The final full green suite ran after restoration. No source edits followed it.

Wire fixture provenance and pinned upstream hashes are in `tests/fixtures/README.md`.
The actual untouched-TS recorder output is `target/slice3-oracle.txt`.
Earlier leaf proofs remain 151/151 JSON cases and 4,844/4,844 output cases.
Earlier Slice 2 sensitivity and fidelity evidence remains under `target/slice2-*`.

## Fidelity limits and next action

- The Rust suite is representative, not an execution of every upstream TS test.
  Complete malformed-input, allocation, GC and SQL statement-count parity is unproven.
  Live reads currently rerun plans per commit and search prior rows linearly, rather
  than reproducing the upstream incremental invalidation/resource-cost algorithm.
- Normal upload batches retain commit objects and compressed bytes within the batch
  bound, plus an adaptive input page. Archives stream. A single oversized commit and
  the initial 100-row page are not a hard 1 MiB memory cap, matching upstream paging.
- Transport ports are host-supplied fallible structural interfaces, not standalone
  OS/WebSocket connection implementations. Host/network binding belongs to 2.1b.
  Applications must acquire/release owner claims and drive the host-wide heartbeat.
- `JsonValue::to_serde` cannot retain lone surrogates. UTF-8/schema/address/typed-write
  bridges do not establish arbitrary JS string parity. Some public Rust configuration
  fields remain forgeable. Existing vector/IndexMap copying and WeakMap ephemeron
  differences remain, as do prior leaf benchmark regressions and eager key enumeration.
- Tokio-backed APIs require a Tokio runtime. Cancellation/error reasons use strings,
  not arbitrary JS error identity. Synchronous user callbacks must return promptly.
- The additive `pi-rs-telemetry` recorder/storage bridge is now landed (`TelemetryRecorderSink`,
  `FallibleTelemetryStore`, `SqliteTelemetry::telemetry_context()`, and `tests/telemetry_bridge.rs`).
  SQLite errors propagate explicitly via `Result<_, crate::Error>` without swallowing failures.

Next: separately claimed Milestone 2.1b (`host`, `jobs`, `registries`, `remote`, `surfaces`, `tui`,
`node/{http,workers,plugins,surfaces,tui,app}`, `web`).
Do not interpret this bounded checkpoint as proof that the known parity limits vanished.
