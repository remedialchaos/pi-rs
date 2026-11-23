# Milestone 2.1a foundation checkpoint

Milestone 2.1b-2 claimed by Jcode on 2026-11-22. Partial checkpoint verified below.
The complete milestone remains open. Do not unblock dependent packages.
The task's explicit scope forbids editing shared MAP/HANDOFF or running map --write.
No commits or pushes. Existing uncommitted work is retained.

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

## Milestone 2.1b-1: portable runtime core

Updated 2026-11-22 UTC. Implementation and bounded functional checkpoint, not a
claim of complete upstream API or resource parity. No commits or pushes created.
Only this crate's `src/`, `tests/`, and this record were authored for this task.
Concurrent workspace manifest, lockfile, sibling-crate and map changes were left alone.

### Gates

| Gate | Result | Evidence |
| --- | --- | --- |
| Formatting | PASS | Exact package-scoped brief command, `chord-b1-final3-fmt.txt` |
| Warning-free lint | PASS | Exact offline brief command, all targets, zero warnings, 1m44s |
| Complete offline suite | PASS | Final exact isolated-target command: 143 passed, 0 failed, 0 ignored |
| Foundation preserved | PASS | All 107 existing tests retained and passing, including 19 wire goldens and telemetry bridge |
| Registries | PASS, bounded | 6 tests: owner order/removal, adapters, follow, atomic replacement, failure retention, conflicts |
| Jobs | PASS, bounded | 7 tests: validation/dedupe/concurrency, cancellation, finish retries, durable recovery/claims, restart limits, state schemas |
| Host | PASS, bounded | 10 tests: lifecycle, admission/follow/revoke, cached sources, document persistence, fixed-point holdback, dry-run declarations, real drain cycle, atomic registry handover |
| Remote | PASS, bounded | 3 real memory-link tests: calls/deltas/rebind, principal stamping/cancellation, atomic paired updates beyond acknowledgement window |
| Surfaces/TUI/web | PASS, bounded | 10 tests: persisted choices, digest identity, theme/language, terminal bridge/scopes/optimism, pure page and connection effects |
| Mutation sensitivity | PASS | All 36 new tests fail on behavioral mutations, with clean source restoration |
| Source safety and scope | PASS | Crate forbids unsafe; no source unwrap/expect/panic/allow, clean whitespace, only permitted authored paths |
| Complete upstream runtime parity | NOT COMPLETE | Specific remaining boundaries below |

### Implementation

- Typed registry definitions, owner tables, contribution/value adapters, failure
  episodes, followed contributions, stable sources and batched host replacement.
- Durable and memory job queues with UUIDv7 run IDs, validation-before-dedupe,
  concurrency limits, FIFO state settlement, generation fences, independent wait
  cancellation, committed intent/outcomes, crash recovery, abort and finish backoff.
  Persisted run/state documents share one store step and owner claims.
- Facets, scoped references/calls, admissions, routines, follow/open resources,
  revocation, presence/activity, hosted documents/tables/jobs and retained link peers.
  Reload rehearses declaration-only factories, iterates holdback to a fixed point,
  commits declarations in one batch, drains old calls, and cuts real old-work cycles
  immediately with `unavailable/reload_cycle`. Cached readable proxies survive restarts.
  Non-native contributions remain through drain or a failed successor and swap in
  the successor's publication batch. Native-open contributions withdraw before
  resource cleanup. Ending an object also removes its retained contributions.
- Direct remote peers use the existing channel protocol, indexed methods/members,
  per-member delta dictionaries, cancellation, child-principal stamping, reconnect
  re-holds and a two-message replica acknowledgement window. Related member deltas
  from a host batch publish atomically. Local contracts cannot cross links.
- Portable scripts/choice contracts, persisted principal-keyed preferences capped at
  10,000 entries, digest adoption/holdback, page state, theme/locale followers,
  translation catalogs, terminal colors, deterministic connection effects, bridge
  requests/sockets, UTF-16-safe terminal frames, `pi-rs-tui::Terminal`, scoped watches
  and optimistic actions. Web page/script/style operations are pure effect data.

### Verification evidence

Tests added: 36. Tests removed: 0. Baseline: 107 passing, 8.61s summed execution.
Final: 143 passing, zero failed or ignored, 11.98s summed execution after 3m46s
compilation. All 36 added tests have observed behavioral failures. Execution totals
exclude builds and are not comparative benchmarks on this concurrently used machine.
Exact final command outputs: `chord-b1-final3-{fmt,clippy,tests}.txt`.
Evidence files are under `$HOME/.jcode/scratch/chord-b1-*`.
The required `/tmp/pi-rs-target-jcode` target is a symlink to session-owned build
storage after the 4 GiB tmpfs filled. No manifest or dependency changes were made.

`chord-b1-mutants1.txt` records 23 failures across registry admission/order/adapters,
follow/retention/conflicts, job cancellation/dedupe/crash/finish bounds, holdback/cycle
cutting, principal stamping, replica atomicity, digests, themes, catalogs, connection
backoff, UTF-16 framing, optimistic settlement and CSS adoption. The second bounded
round (`chord-b1-mutants2b.txt`) checks presence, cached handles, rejected rehearsals,
persisted document binding, state-schema identity, committed job outcomes, abort
classification, choice persistence, script contracts and bridge reply data.
The shared-source teardown mutation initially survived a fast reconnect, so that
test now waits for actual object cleanup before re-holding. It then fails in
`chord-b1-mutant-rebind.txt`. Every new test is listed in the 35-line
`chord-b1-mutation-coverage.txt`. The original unbounded mutated document wait was
stopped and readiness/recovery assertions gained explicit deadlines. Both source
restorations passed `sha256sum --check` against `chord-b1-before-mutations.sha256`.
An intermediate full-suite attempt exposed a cached-reference revocation bug: the
fix fails/ends owned proxies without detaching borrowed factory sources. The
expanded admission test covers deny, reallow and revoke on a cached source.
A 36th test initially failed on a non-native contribution gap, then passed after
atomic successor handover. A native-seal mutation failed its other branch;
`chord-b1-handover-{red2,edges,mutant}.txt` records these results, and restoration
matches `chord-b1-handover-clean.sha256`. Failed successors and final release are
covered by the same real host test.

### Honest fidelity limits and next work

- This is representative Rust integration coverage, not replay of the entire TS
  application or upstream runtime test suite. A Wasm target was not built. Node,
  browser DOM, process, HTTP and OS connection adapters are not part of this slice.
- Host/remote expose JSON state and methods. Event, keyed/query member integration,
  route/destination/relay resolution, origin forwarding, bytes-valued calls and
  remote historical contract views are not implemented. Accepted host contracts
  bind by compatibility, but do not reproduce the full upstream adaptation layer.
- Job finish callbacks and domain-document writes are not one atomic transaction
  with run retirement. In-memory job records/waiters are not handed across object
  restarts. Stored state schema migrations, missing-declaration recovery, blocked-run
  operator retry and durable trace-carrier restoration remain incomplete.
- The declaration commit and object contribution handover have separate upstream
  boundaries: facet declarations commit together, retained object contributions
  swap when a successor goes live. The closure check is conservative for unrelated
  unbound requests after a failure. Cancellation after commit does not expose an
  adoption-ready committed report to LoadedFacets.
- Registry composition is eager rather than upstream lazy. Last-good ownership,
  failure/end propagation and report deduplication have narrower coverage. Host
  follow failure/end does not reproduce every attempt-restart rule; job activity
  changes do not independently republish the host activity snapshot.
- Replica messages waiting for acknowledgement are queued rather than coalesced
  under a hard memory bound. Separate initial binds are not aggregated across
  objects. Large-peer and hostile-peer scheduling/resource parity is unproven.
- Locale canonicalization/direction uses a bounded Rust implementation, not full
  Intl/CLDR data. Number/date/time formatting is not ported. Theme/translation registry
  facet helpers and complete fallback-report deduplication remain to be wired.
- Connection and web modules emit adapter effects rather than running an actual
  browser/launcher. Stylesheet timeout driving, complete bootstrap/environment
  wiring and native GUI behavior are not established by these tests.

## Milestone 2.1b-2: native platform checkpoint

Updated 2026-11-22 UTC. This is a partial implementation, NOT completion of
Milestone 2.1 and NOT permission to treat its dependent packages as unblocked.
Only this crate's `src/`, `tests/`, `wit/`, and this record were authored.
Shared manifests, lockfile, tracking files, upstream sources and sibling crates
were not edited. Existing dirty work was retained. No commit or push was run.

### Acceptance gates

| Gate | Result | Evidence |
| --- | --- | --- |
| Formatting | PASS | Exact brief command, `chord-b2-final-fmt.txt` |
| Warning-free lint | PASS | Exact offline all-target command, zero warnings, 50.37s wall time |
| Complete offline suite | PASS | 163 passed, zero failed/ignored, 9.32s summed execution, 325.52s including build |
| Compression, HTTP and live WebSocket RPC | PASS, bounded | Raw deflate bounds/corruption, live admission/wraps/headers, signed cookies, file containment/ranges, remote call and readable updates |
| Plugin component engine and reload | PASS, bounded | Real Component instantiation, checked-in WIT bindgen, native SHA identity/reuse, holdback, discovery and filesystem notifications |
| Native worker lifecycle | PASS, bounded | Actual Rust child demand/start/plugin delivery/echo/idle stop/crash retention, IPC bounds and kernel lock exclusion |
| App boot, metadata and native file persistence | PASS, bounded | Server/worker boot, AppInfo/WorkerInfo, owner/pair/invite/ephemeral HTTP flows, restart, versions/uploads/folders and archive staging roundtrip |
| Surface HTTP, host scripts and terminal protocol | PASS, bounded | Real HTTP artifact and Host scripts readable, holdback, fragmented framing, filtering and private credentials |
| New-test mutation sensitivity | PASS | All 20 failed under deliberate behavior faults, all 17 source/WIT hashes restored, final 163 passed |
| Complete required platform orchestration | FAIL | Address data management, registry adapters, build/bootstrap and launcher integration remain below |

### Implemented boundaries

- `node::compression`: raw DEFLATE level 1 and bounded complete-stream inflate.
- `node::http`: native Axum HTTP/WebSocket listener, ordered routes/wrap phases,
  authenticators, origin/Host/relay admission, CSP and security headers, HMAC cookies,
  canonical static-file containment, ETag/ranges/HEAD, binary links into Host RPC.
  Output queues and frame sizes are bounded. Ephemeral socket leases release on drop.
- `node::plugins`: compiled-in facets and Component Model guests with checked-in
  `wit/plugin.wit`, JSON state/method declarations, fuel and store limits, no WASI,
  SHA-256 identities, failed-successor holdback through the existing Host staging,
  directory/package discovery and debounced bounded native filesystem notifications.
- `node::workers`: framed native child IPC, digest-checked plugin shipments,
  demand-start handles, readiness/idle/shutdown deadlines, sticky crash failure,
  explicit retry and private kernel-owned advisory lockfiles. Worker sessions own
  their pump tasks and release process locks on drop.
- `node::surfaces` and `node::tui`: content-addressed prebuilt artifacts, retained
  old publications, script manifests/HTTP serving, native pipe framing, output
  filtering, create-only private credentials and `pi-rs-tui` launcher actions.
- `node::app`: private layout/store setup, server/worker lifetime, metadata facets,
  persisted redacted sign-in listing and token hashes, one-use owner/pair/invite
  flows, short-lived logins, native versioned Files/FilesFolder/blob/upload APIs,
  checksummed bounded tar parsing and validated archive-to-staging import.

### Verification evidence

Tests added: 20. Tests removed: 0. Baseline: 143 passed, 8.66s summed test execution
and 531.17s including the initial dependency build. Final: 163 passed, zero failed
or ignored, 9.32s summed execution and 325.52s including rebuilding the crate's test
binaries. Execution times on this shared machine are not performance comparisons.
All three exact brief commands passed in `chord-b2-final-gates`, with the required
`/var/tmp/pi-rs-target-jcode` target directory and offline flags.
Logs are in `$HOME/.jcode/scratch/chord-b2-*.txt`.

Mutation campaign: all 20 new tests failed with deliberately broken bounds/corruption
classification, cookie MAC verification, cache headers, range status, WebSocket auth,
plugin discovery/digests/component admission/watcher delivery, worker bytes/locks,
surface holdback, terminal bounds/credential replacement, file reads and tar checksum.
Faults were applied together, not independently scored. App boot tests detected the
lock fault before the additional metadata mutation. All 20 test names reported
failure. SHA-256 checks restored every one of the 17 recorded source/WIT files
byte-for-byte before the successful final run. The fault run took 37.56s including
rebuild. The checked-in standalone worker fixture is Rust and was exercised as an
actual child process, not a process mock.

Acceptance runs found and corrected root document writes with empty paths,
non-idempotent Files index declarations on reopening, incorrect local contract
variants, delayed terminal frame-length validation, and a worker test that assumed
control readiness preceded the binary link hello. Readiness tests now have deadlines.

### Honest fidelity limits and next action

- Full upstream reference/test replay and complete external-contract equivalence
  have not been established. The prior Host/remote JSON-only, keyed/event/bytes,
  destination routing, job activity and historical-contract limits still apply.
- Native HTTP contributions are callback APIs, not an implementation of every
  upstream Http registry facet. Contributions close all retained links on changes.
  WSS client support is not enabled by the supplied dependency features.
- Guest declarations currently support local singleton JSON state and methods.
  The offline component test instantiates a real empty Component Model binary and
  rejects missing plugin-world exports. It does not execute a full guest plugin.
  Per-host reload uses Host staging, but rollout over multiple runtimes is sequential,
  not one atomic distributed publication. Discovery does not implement all globs,
  install/build conventions or the complete upstream package metadata model.
- Surfaces accept already-built Rust/WASM/native artifacts. A Rust-to-WASM build,
  actual browser bootstrap and full native terminal subprocess/TTY event loop are
  not implemented. Clipboard/open/attach/fetch actions remain caller-driven effects.
- AppServer does not compose worker-kind routing/demand or HostData owner retirement.
  Users, host-bound Files/FilesFolder keyed/bytes facets and per-address running-worker
  export, active import/swap and delete are not implemented. Native Files methods do
  not substitute for those external contracts. Folder views lack upstream paging
  and persisted summary documents. Upload expiry is explicitly driven by the caller.
- Archive APIs buffer whole entries instead of streaming. The staging roundtrip
  restores the full supplied store and finished blobs, but optional `server.archive`
  is only carried as bytes. It is not a complete format-2 host ownership transfer and
  must not be represented as compatible app export/import/delete per address.
- Linux was exercised. macOS/Windows compilation, ownership checks beyond Linux,
  filesystem race hardening and upstream resource-cost parity remain unverified.

Next: finish the FAIL gate, starting with HostData address ownership operations and
worker routing, then registry-facing Files/HTTP and real launcher/build/bootstrap
wiring. Do not mark Milestone 2.1 complete from this bounded checkpoint.

## Milestone 2.1b-3: platform orchestration

- `AppServer` routes authenticated holds to address-scoped workers on demand.
  `HostData` exports running workers without stopping them. Import stages the
  format-2 archive before retiring and replacing the target; delete retires only
  the selected owner. Tar entries stream to and from disk, including the worker
  store and finished blobs. The server store separately imports or deletes only
  rows and documents that belong to that address.
- The Host HTTP registry publishes routes, wraps and sockets on the running
  listener and retires links owned by a removed contribution. Host-bound
  `Files`, `FilesFolder` and `Users` expose paged views, persisted profiles and
  idle upload expiry. Surface bootstrap and artifacts use the same HTTP registry.
- Checked-in Rust-compiled WASM components exercise the production WIT setup,
  method and state publication path. The rollout test proves successful
  replacement and retention of the last healthy guest when setup is rejected.
- The TUI launcher fetches authenticated bootstrap and content-addressed assets,
  constrains fetches to its surface, and connects and closes its Chord client
  link through the launcher channel.

### Verification

| Gate | Result | Evidence |
| --- | --- | --- |
| Formatting | PASS | Crate `cargo fmt --check` |
| Offline strict Clippy | PASS | All targets, `-D warnings` |
| Offline Cargo suite | PASS | 178 passed, 0 failed, 0 ignored after restoration |
| Mutation sensitivity | PASS | All 15 new tests failed under combined behavioral faults; all 10 mutated source files restored and verified by SHA-256 |

Added 15 offline integration tests and removed none. Summed test execution grew
from 9.32s (163 tests) to 30.41s (178 tests), including real worker child processes
and guest component execution. Cargo test wall time including rebuild was 433s
on the final run, versus 325.52s on the prior checkpoint.

### Remaining limits

This verifies the Rust orchestration paths, not full upstream or Phase 3 parity.
Files transports bytes and cursor pages through JSON contract methods. The TUI
launcher supplies a native client session rather than starting an arbitrary
artifact as a subprocess or owning the operating system TTY. Scoped server
store replacement is transactional and ordinary swap failures restore the old
directory, but the filesystem and SQLite do not share a crash-atomic commit.
WASM reload holdback is per host, not a distributed commit. Upstream replay,
WSS client support, and non-Linux verification are not covered by this run.
