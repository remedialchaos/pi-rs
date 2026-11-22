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
