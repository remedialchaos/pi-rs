# Milestone 1.2: telemetry worker handoff

Base commit: `780bf31fc`. Worker: `t3q`. Verified: 2026-11-22 UTC.
No commits created. Coordinator-owned tracking files were not edited.

## Acceptance gates

| Gate | Result | Evidence |
| --- | --- | --- |
| Renderer recorded oracle | PASS | 210/210 cases match, 66/66 upstream lines reached |
| Renderer build | PASS | `rust-anything build rust/pi-rs-telemetry-render` |
| Renderer gate | PASS | fmt, clippy, tests, audit, API, behavior, zero explicit unsafe |
| Renderer benchmark | MEASURED | 0.27x including N-API boundary, not a speed optimization |
| Standalone fmt | PASS | `cargo fmt --check` |
| Standalone clippy | PASS | `cargo clippy --all-targets -- -D warnings` |
| Standalone runtime tests | PASS | 53/53, including all 52 upstream runtime contracts |
| Schema compile-fail tests | PASS | 2/2, required start attributes and metric-kind bounds |
| Real process propagation | PASS | Parent continuation through the test worker binary |
| Untouched upstream | PASS | `git diff --quiet -- packages/ apps/`, exit 0 |

Final combined verification unit: `pi-telemetry-final-check.service`, exit 0.
Its journal retains commands and results. The transient unit was collected.

## Implementation

- Standalone edition-2024, unpublished library with no N-API dependency.
- In-memory spans, explicit parents, detached snapshots, events, links, status, synchronous admission, and owned callback futures.
- No-op context, propagation and metric handles retain inert identity.
- Counter, up/down counter and histogram instruments, metadata factories, exemplars, and settled store queries.
- Wall/monotonic anchored microsecond clock, inclusive query bounds and stable cursor pagination.
- Six runtime test suites: conformance 15, render 11, metrics 4, telemetry 5, store 12, propagation 6.
- `src/render/verified.rs` exactly matches the renderer proof's production implementation. The adapter preserves numeric property enumeration, undefined attributes and nonfinite timestamps.
- `tests/support/propagation_worker.rs` is a process-boundary fixture, not a shipping front door.
- Dependency research is in `rust/research/telemetry.md`.

## Verification details

Added 53 standalone runtime tests, 2 compile-fail doctests, 9 proof integration tests and 1 proof inline regression. Removed none. The generated JS-compatibility helper also has 3 unit tests.

Before this work, the standalone crate and its suite did not exist, so there is no pre-port runtime baseline. The first complete 52-contract run took 0.14 seconds. The final 53-contract run took 0.16 seconds, doctests 0.11 seconds, and cargo test including compilation 8.32 seconds.

Temporary behavioral mutations were exercised and restored. One batch broke settled mutation guards, propagation identities, clock precision, metric values/names/identity and renderer output: 33 tests failed. A second batch dropped measurement attributes and terminal escaping: 6 tests failed. The inline negative-half rounding regression failed with `-0.001ms` instead of `0ms`. The extra standalone serialization regression was observed failing against the old adapter, then passed after the fix. These are sampled mutation checks, not an exhaustive per-test mutation score.

## Integration notes and limits

- The proof uses the generated loader's existing JSON fallback. Binary-buffer negotiation returns NaN to disable its optional optimization, avoiding unsafe buffer aliasing. `rust-anything refresh` would overwrite this safe binding and must not be applied blindly.
- Rust schema marker traits and caller-defined attribute structs/event enums replace TypeScript literal inference. Runtime JSON schema helpers remain identity/metadata operations, as upstream does. Automatic type generation from schema JSON and duplicate-name type inference are not implemented.
- The current context/capability facade and conformance fixture use concrete reference-adapter handles. Implementing a separate non-memory recording backend will require generalizing those handles. The 52 upstream runtime contracts are verified on the in-memory and no-op adapters, not on an additional backend.
- Verification was on Linux x86_64. Other targets were not built in this milestone.
- Coordinator should update MAP/HANDOFF and run the repository-wide map checkpoint when accepting the milestone. This worker did not alter those shared files.
