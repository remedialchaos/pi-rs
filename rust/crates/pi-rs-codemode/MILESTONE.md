# Milestone 1.4: codemode ADR-010 handoff

Status: implementation complete, worker verification PASS. Coordinator t3h owns acceptance and integration.
Worker: t3q (Jcode). Repository base: `780bf31fcaeb252fa9b68df3964c735eae75bde3`.
Evidence: 2026-11-22 UTC. No commits, proof-leaf edits, upstream edits or shared tracking edits.

## Acceptance gates

| Gate | Result | Evidence |
| --- | --- | --- |
| Accepted proof leaves retained | PASS | Identifier 246, source 244, declarations 684. All 1,174 cases replay through native Rust public functions. Earlier 3/3 proof gates remain accepted, not rerun or modified here. |
| Real QuickJS-WASM execution | PASS | Wasmtime 50.0.2, pinned quickjs-wasi 3.6.2 guest, async host calls on all three backends |
| Catchable recursion | PASS | Original >1,000-frame RangeError acceptance test, plus worker/inline/remote contracts |
| Heap byte limits | PASS | Worker allocation recovery and remote 8 MiB allocation failure |
| Deterministic execution | PASS | Clock, pinned randomness, UTC, no weak references, fingerprint, ordered delivery, idle/during, rejection properties |
| Inline/remote execution | PASS | Accumulated interrupt budget, JSON transport, reordered replies, output batching, server caps and Stop disposal |
| Worker lifetime | PASS | Admission failure, module failure before admission, actual-exit release, failing lifetime hook, cancellation before actual exit, real child process exit |
| Six-file upstream coverage | PASS, native adaptations | `COVERAGE.md` maps behaviors and identifies Node/Bun plumbing and Rust type-boundary adaptations. Not a claim of 231 identically named Rust tests. |
| Formatting | PASS | `cargo fmt --check` |
| Strict lint | PASS | `cargo clippy --all-targets -- -D warnings` |
| Whole native suite | PASS | `cargo test`: 273 passed, 0 failed, 0 ignored, doc tests 0 |
| Untouched upstream | PASS | `git diff --exit-code -- packages/ apps/` |
| Temporary runtime units | PASS | All `pi-codemode-*` collectible units finished, none remain |

Final restored verification: `pi-codemode-final-0351.service`, exit 0 at 04:01:23 UTC.
Per-binary counts: library 1, ADR-010 176, boundary 23, recursion 1, data 9, declarations 14, parity 15, runtime 10, sandbox 21, lifetime 3.
Test-body time: 220.41 seconds. Test compilation: 363 seconds. Full unit: 589.40 seconds.

## Delivered implementation

- `guest.rs`: safe checked Wasmtime guest memory/handle ABI, custom WASI imports, compiled module cache, fresh Store per run, QuickJS stack/heap/interrupt guards, deterministic clocks/randomness and ordered result application.
- `runtime.rs`: shared execution state, worker/inline launchers, concurrent host futures, overall deadline including initialization, cancellation, partial results, actual-exit admission accounting and host-controlled runs.
- `remote.rs`: transport-neutral client/server exchange, batching and host caps. Host tool implementations stay on the caller side.
- `build.rs`: Rust-only extraction of the unchanged upstream prelude snapshot. No replacement JavaScript authored.
- Public identifier/source/schema modules still reuse the accepted sibling proof sources. Existing public data types and 60 native tests retained.
- Obsolete Boa dependency, implementation and limitations removed. Temporary logs removed after recording this evidence.

## Asset fidelity

SHA-256 values, compared directly with installed upstream assets and the evaluated upstream prelude:

- WASM: `d4c9375f2b1ca4dc95f72c8aa2982a7a9951ac8011490d79c6582df732b4bbd9`.
- MIT license: `f7735da3b2531ddf451937117343cfadb853fd8054b499e7a70695dc57ffd184`.
- Evaluated prelude: `e64ad1a6afffbfb8f75a0d08434a9a3fec88d1e54206b03953bfc7b5820cc119`, 17,344 UTF-16 code units.

## Behavioral verification and cost

Added 213 native tests to the retained 60. Removed 0 tests. Before ADR-010: 59/60 passing, 6.65 seconds test-body time on Boa. After ADR-010: 273/273 passing, 220.41 seconds. Coverage and runtime changed, so this is not a comparable performance benchmark or a performance improvement claim. Each integration test process recompiles its in-memory Wasmtime guest once.

Focused behavioral mutation trial: changing deterministic seeding, undefined wire encoding, bridge validation, actual-exit ordering, remote Stop cleanup and declaration exports caused 15 boundary, 14 declaration and 2 deterministic test failures. Mutations restored, then the entire suite passed. Earlier red runs caught the inline budget-after-await bug and module-load-before-admission bug. A final red/green regression caught the embedded-vs-empty-explicit-path cache-key collision. These are focused checks, not exhaustive independent mutation coverage of every assertion.

## Integration notes and hazards

1. Unified workspace member (`rust/crates/Cargo.toml`). The three proof modules (`src/{identifier,schema,source}.rs`) and their 1,174 recorded cases (`tests/fixtures/{identifier,source,declarations}.jsonl`) have been inlined into `rust/crates/pi-rs-codemode/` with zero external relative `../../../pi-rs-*` paths (`cargo test -p pi-rs-codemode --lib --test data_contracts --test declaration_contracts`: 24/24 PASS).
2. Wasmtime uses a JIT and its default host platform backend. Linux x86_64 is verified. Other configured target platforms are not live-tested here.
4. The cap is QuickJS's heap byte limit, not a cap on total process RSS or Wasmtime compilation memory. Host tools grant only the capabilities their caller supplies. This is not a formal security audit.
5. Worker scripts use a 512 KiB QuickJS stack and inline/remote scripts 256 KiB. The underlying Wasmtime stack ceiling is 2 MiB and native worker thread stack 4 MiB.
6. Overall wall deadlines include module loading and VM initialization. Module compilation may finish after a cancelled caller, without taking worker admission. Cache successes are shared, failed loads are not cached.
7. Native options replace JavaScript worker-file URLs with compiled Rust worker launch and optional `wasm_path`. Strong Rust JSON types reject some impossible JavaScript host values before delivery. See coverage adaptations.
8. The retained source leaf reproduces recorded malformed-JSON diagnostics. Unrecorded parser diagnostics are not newly proven identical to V8.
