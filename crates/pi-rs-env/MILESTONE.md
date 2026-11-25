# Milestone 2.3a — `pi-rs-env` daemon integration and client transport

## Scope

- Integrated upstream `packages/env/daemon/src` (16 files) into `src/daemon/` and wired `src/bin/pi_env.rs` to
  `daemon::main_entry()`. Wire protocol, operation set, scheduling, error codes and framing are unchanged.
- Ported the six `packages/env/src` client modules to Rust: `connection`, `ssh`, `process`, `terminal`, `watch`,
  `errors`, plus `framing` (client-side incremental frame codec) and `sha256` (pure Rust, no new dependency).
- Pinned `hello.version` to the fork's npm version `1.6.0` in source; upstream obtains it from `build.rs`, which is
  outside this crate's permitted edit surface.
- Adapted `watch.rs` from `notify 8.2` to workspace `notify 9`: the existing `RecommendedWatcher::new` callback and
  `Event`/`EventKind`/`need_rescan` API compile unchanged; the `f_type` cast was rewritten to avoid a clippy lint.
- Moved every upstream `unsafe` call out of `pty.rs` and `watch.rs` into safe, `// SAFETY:`-annotated wrappers in
  `daemon/sys/unix.rs` (PTY setup, winsize, `poll`, `statfs`, foreground process name), so `unsafe` exists only in
  `daemon/sys/{unix,windows}.rs`.

## Gates

| Gate | Status | Evidence |
| --- | --- | --- |
| `cargo fmt -p pi-rs-env -- --check` | PASS | Clean, no diffs |
| `cargo clippy -p pi-rs-env --all-targets -- -D warnings` | PASS | Offline, `-j 2`, isolated `CARGO_TARGET_DIR=/tmp/pi-rs-target-opencode` |
| `cargo test -p pi-rs-env` | PASS | 33 lib + 8 integration + 0 bin + 0 doc = **41 tests, 0 failures** |
| Upstream daemon leaf tests | PASS | 15/15 (`decode`, `frame`, `list`, `scan`, `window`, `watch`, `sys::unix`) |
| Real end-to-end daemon tests | PASS | 8/8 spawning `env!("CARGO_BIN_EXE_pi-env")` via `local_daemon()` + `Connection` |
| Mutation checks | PASS | 5/5 (below) |
| Quality rules | PASS | Zero `#[allow(...)]`; `unsafe` only in `daemon/sys/{unix,windows}.rs` with `// SAFETY:`; no `unwrap`/`expect`/`panic!` on fallible paths (invariant uses carry `// INFALLIBLE:`) |
| Edit scope | PASS | Only `rust/crates/pi-rs-env/` (`src/`, `tests/`, this file). No `git commit`/`git push`. |

## Tests

**Added:** 33 unit tests (18 new client/framing/sha256/error/ssh tests + 15 preserved daemon leaf tests) and 8 real
end-to-end integration tests:

1. `hello` handshake, protocol/version/features, `node_platform`, graceful close
2. filesystem: `ENOENT`, `write`, `open`/`pread`, `scanLines`, recursive `mkdir`, `listFiles` payload order
3. `exec`: stdout/stderr events, spill file contents, abort/cancel grace
4. `openProcess`: stdin echo, stderr routing, exit code
5. `openTerminal` (Unix): PTY write/read, resize, foreground, abort and exit code
6. `RemoteWatcher`: created/modified/removed change events
7. connection-lost: in-flight request fails with `lost: true`, next request restarts the daemon
8. `local_daemon()` resolves to the compiled `pi-env` binary and can start it

**Removed:** 0.

**Suite runtime:** lib tests 0.89 s, integration tests 0.33 s, total ~1.22 s (isolated target dir, offline).

## Mutation checks

Each mutation was applied to the source, the covering test was observed to fail, and the original source was restored
and re-verified green:

| Behavior broken | Test that failed |
| --- | --- |
| Client frame length written one byte short | `framing::tests::round_trips_kind_id_json_and_payload` |
| `StrictHostKeyChecking=yes` changed to `no` | `ssh::tests::builds_the_hardened_ssh_argument_list` |
| `macos` → `darwin` platform mapping removed | `connection::tests::maps_daemon_platform_names_to_node_names` |
| `ENOENT` no longer maps to `not_found` | `errors::tests::maps_daemon_codes_to_the_durable_file_codes` |
| Daemon frame kind byte incremented | `daemon::frame::tests::round_trips_json_and_payload` |

## Caveats for 2.3b

- `FileError`/`FileErrorCode`, `FileChange`, `WatchTarget` and `WatchOptions` are Rust stand-ins for the durable types.
  The `ExecutionEnv` bridge (`list-files.ts`, `remote-env.ts`) should wrap them rather than re-map error codes.
- `packaged_daemon()` assumes daemon binaries are bundled under a `bin/` directory beside the host binary; the Rust
  port has no npm package `bin/` tree. `local_daemon()` resolves the compiled binary and is the supported local path.
- SSH bootstrap helpers (`detect_platform`, `deploy_daemon`, `scan_host_key`, host-key accept/forget) are ported and
  unit-tested offline, but no live `sshd` integration test was run. The Windows `sys` module is carried over faithfully
  but only compiled on Windows targets; no live Windows/macOS runtime test was possible here (ADR-005).
- `tokio` remains an available dependency for later async front doors; this transport uses std threads and channels for
  deterministic request/event dispatch and drop-in wire behavior.

## Milestone 2.3b — durable execution environments

The public `RemoteExecutionEnv` and `LocalExecutionEnv` implement the durable crate's
`ExecutionEnv`. Both recursively list through the caller's daemon connection. Other
local operations delegate to the durable local environment. Remote operations use
daemon file handles, session-bound reads and writes, shell configuration, output
callbacks, command cleanup and filesystem watch events.

| Check | Result | Evidence |
| --- | --- | --- |
| Formatting and lint | Pass | Crate-scoped Cargo fmt check and all-targets Clippy with warnings denied |
| Native suite, offline | Pass | 33 library tests, 8 daemon integration tests and 4 durable adapter tests on Linux x86_64 |
| Live listing and shell comparison | Pass for the exercised cases | Unmodified TypeScript adapter and Rust adapter against the compiled stdio daemon both returned directory `sub`, file `sub/a`, no truncation, exit code 7, stdout `configured` and stderr `problem` |
| Offline listing contract | Pass for the exercised cases | Both implementations rejected a limit of 2^53 as invalid before checking cancellation and returned aborted for a valid cancelled request |
| Reader and watch integration | Pass | A 600,000-byte open file survived rename and replacement. The polling watcher reported the new file's absolute path |
| Mutation checks | 4/4 detected | Mapping directories to files, swapping stdout and stderr, accepting 2^53 and reporting polling as native each failed its corresponding test. All four passed after restoration |
| rust-anything | Analyze succeeded. Formal diff, bench and gate unavailable | This native crate has neither recorded bridge cases nor a rust-anything.json bridge manifest. Diff reported missing cases. Bench and gate reported the missing manifest |
| App replay | Coordinator-owned | Coordinator reported 6/6 for the root p2 HTTP oracle. This crate run did not replay p2 |

Four tests added, zero removed. The original crate at `1e6e4aa16` passed all 41 tests.
Measured execution times excluding builds were 1.37 s before (1.05 s library, 0.32 s
daemon) and 1.78 s after (0.89 s library, 0.82 s daemon, 0.07 s durable).
The baseline library timing was taken from a separate successful run of the same
binary with the assigned temporary directory. An exploratory invocation without
that directory failed the pre-existing gitignore fixture. The offline test build
with the durable dependencies took 17 min 22 s.

The compatibility comparison covers the listed examples. Both adapters returned `not_supported` with the same update message against the
unmodified upstream old-daemon fixture. Cross-platform path behavior, live SSH,
transfer throughput and comprehensive differential replay remain unverified. Remote
file transfers use sequential chunks, while the upstream client pipelines them.

### Async transport review

Filesystem requests and handle acquisition now run on Tokio's blocking pool.
The acquisition worker owns the returned handle before handing it to the caller.
Explicit close awaits a worker, and Drop schedules best-effort close using the
original daemon session and handle ID. Watch setup, watcher joins and shell
cleanup also run outside the executor thread.

The new Unix current-thread Tokio test starts the compiled daemon and reads a
FIFO. A heartbeat cancels the context and releases the writer while the daemon
is waiting for data. The read returns the durable aborted error. The test failed
on the original implementation and again when only the shared request was made
synchronous (2.01 s watchdog). It passed after restoration.

Formatting, all-targets Clippy with warnings denied, and all 46 crate tests
passed offline. One test added, zero removed. Test execution totaled 2.39 s
(1.56 s library, 0.77 s daemon, 0.06 s durable), versus 1.78 s in the prior run.
The existing disconnect test now waits for request start before killing the
daemon. Its former ordering sometimes killed the daemon before the request
started, allowing the request to succeed on a replacement session.
### Daemon-backed listing proof

A clean export rebuilt the production daemon and reproduced the recorded local
and remote listing cases. Both scenarios matched across five operations each.
Changing the fixture root made both comparisons fail. The formal gate passed
with a Cargo audit advisory warning. Upstream line coverage was 765/1,808
(42%). On the unoptimized verification build the five-operation scenarios
averaged 30.62 ms upstream and 23.91 ms in Rust, including connection setup.
The retained fixture, corpus and [reproduction recipe](../../verification/env-listing/README.md)
cover ordered entries, truncation, invalid limits, filesystem and cancellation
errors. This proof adds two recorded scenarios and zero native tests.
