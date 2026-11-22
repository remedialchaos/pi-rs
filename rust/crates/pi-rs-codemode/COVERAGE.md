# Native acceptance coverage

Upstream inventory: 231 test instances across six files. The Rust suite has 273 tests, including the retained 60 tests and 213 additions for ADR-010. Counts differ because assertions are grouped differently and JavaScript runtime plumbing has native equivalents. Passing the upstream oracle during earlier recording is not counted as a native test.

## Mapping

| Upstream file / group | Native acceptance location |
| --- | --- |
| `sandbox.test.ts`: 48 shared contracts on worker, inline and remote | `adr010_contracts.rs`: 48 named contracts instantiated for each of `worker`, `inline`, `remote`, 144 tests |
| Prelude parses as JavaScript | All actual guest initializations evaluate the exact prelude, plus native host-await smoke. Generated prelude compared byte-for-byte with evaluated upstream PRELUDE_SOURCE. |
| Worker module loading / broken bridge | `boundary_contracts.rs` malformed writes, return/error JSON, unknown wire variants, late message settlement, explicit WASM paths. `adr010_contracts.rs` failed load before admission. |
| `deterministic.test.ts`: replay and host-controlled delivery | `adr010_contracts.rs`: 13 named contracts on worker and remote, transport-reordering test. `boundary_contracts.rs`: real clock/randomness and isolated non-UTC-host child process. See type adaptation below. |
| `portable.test.ts`: inline budget and remote transport | `adr010_contracts.rs`: synchronous/microtask budgets, cumulative budget across host waits, caller-side tools, output batching, server budget/heap caps, failed/silent/unreachable remotes. `boundary_contracts.rs`: abort completes server and drops VM. |
| `declarations.test.ts`: 19 contracts | `declaration_contracts.rs`: 14 expected-output tests, plus five declaration-front-end tests in `data_contracts.rs`. Includes every upstream primitive/union/object/comment/constraint/reference/array/budget example, MCP-shaped output and full tool/global declarations. |
| `source.test.ts`: five contracts | `data_contracts.rs` plain/directive/newline and invalid-input tests, `boundary_contracts.rs::source_exact_directive_contracts` exact rejection fragments and misplaced-line numbers. All 244 source recording cases replay natively. |
| `worker-lifetime.test.ts`: admission, release, cancellation, process exit | `worker_lifetime.rs` existing tests plus `boundary_contracts.rs` deterministic actual exit, gated exit hooks, abort/timeout/close settling before exit, host token cancellation and native child process exit. Failed WASM loading takes no admission. |

## Shared sandbox contracts

The three backend modules exercise JSON returns, top-level await, ordered text/image/console output, non-finite output, invalid output arguments, large wrapped images, exit and first settlement, partial output, syntax/thrown line numbers and stacks, non-Error throws, non-serializable returns, sequential/concurrent tools, normalized aliases, undefined/null, catchable tool errors, unknown tools and suggestions, unawaited-call cancellation, registration, detached store snapshots/copies, per-value/aggregate store caps and hints, reserved globals, top-level/namespaced/spread globals and suggestions, invalid names, synchronous/microtask timeout, disabled deadlines, stalled promises, abort/close, aggregate output/item caps, closed rejection, parallel isolation, catchable recursion, absence of host globals, confined eval/Function, dynamic-import rejection, intrinsic lockdown including instance-only intrinsics, instance overrides, non-string error fields and frozen tool/console objects.

## Deterministic contracts

The worker and remote modules exercise reproducible calls/output/clock/randomness, UTC and absent WeakRef/FinalizationRegistry, out-of-order result buffering, startup fingerprint-before-calls, ordered idle/issued events, common call numbering for globals/tools, monotonic finite clock validation, non-object rejection properties, unknown/non-waiting calls and reused/zero sequence numbers, finished state, uncaught external abort, terminal `during`, rejection property preservation without name/message overwrite, explicit script names and normalized-name collisions. Seed-zero and changed-seed/start-time assertions are included. A JSON-cloned remote deliberately delays sequence 1 to let sequence 2 arrive first.

## Native boundary adaptations, not extra passed upstream instances

- JavaScript worker paths and missing JS worker files have no direct native option. Rust starts its compiled worker closure and accepts an explicit QuickJS WASM file path instead. Valid, missing and empty explicit paths are covered. Native OS thread-construction failure releases captured admission through RAII, but actual OS resource exhaustion is not injected in the suite.
- Node `--watch` dependency notifications and Worker `ref`/`unref` calls do not exist on the typed Rust thread channel. Unknown remote JSON variants are rejected by serde. Malformed payloads that the native wire type can express go through the actual shared host parser and settle with sandbox errors.
- The upstream host-answer serialization test supplies JavaScript BigInt as a successful answer and nested BigInt as rejection properties, then retries the same sequence/call. Native `Outcome` holds `serde_json::Value`, which cannot contain BigInt or cyclic objects, so these invalid host states are excluded before delivery. Invalid JSON-compatible rejection properties are tested to leave the same sequence/call waiting and allow a valid retry. Guest BigInt/cyclic return and argument failures are tested separately. No runtime BigInt host-answer test is claimed.
- Node and Bun process-lifetime rows are represented by native Rust child processes using real VM workers. Abort, timeout and close all settle before a deliberately delayed admission-release hook, then the child exits. No Node/Bun process is required or silently skipped. Child timezone is set by process environment, not unsafe mutation of the multithreaded test process.
- Platform-specific Node/Bun worker constructor/watch mechanics are not claimed byte-for-byte Rust API compatibility. The JavaScript scripts, prelude, JSON bridge data and observable execution results retain the upstream contracts covered above.

## Regression strength and remaining validation scope

Focused mutants caused 15 boundary, 14 declaration and two deterministic failures. The explicit empty-path cache regression failed before its fix. Inline budget-after-await and module-loading/admission regressions also failed before their fixes. All mutants were restored before the final 273-test run. Not every new assertion has an independent mutation result.

The native runtime is tested on Linux x86_64 only. Remote execution is tested over a serialized in-process transport, not a deployed network or Cloudflare service. The shared workspace/application front door is outside this worker's assigned scope. No security certification, exhaustive fuzzing or runtime speedup is claimed.
