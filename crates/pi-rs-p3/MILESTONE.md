# Milestone 3.1 checkpoint: `pi-rs-p3`

Milestone 3.1 ports the live `apps/p3` server, worker, supervisor, `v1` API, and the four built-in
plugins. `apps/p3/old/` was confirmed dead archive code (Open Question #1) and is not ported.
No commits or pushes were made. Every authored change is inside
`rust/crates/pi-rs-p3/` (plus its `tests/`); `packages/`, `apps/`, the workspace manifests, and
sibling crates are untouched.

Worktree HEAD `18bd5be94` on `wt/p3`. Upstream reference `780bf31fc`.

On 2026-11-22 the takeover run passed all three final Cargo gates. The warm test
run took 2.41 s wall time (library 1.08 s, CLI 0.25 s, runner 0.17 s).
The takeover added and removed zero tests. All five restored source hashes
matched the prior mutation log. Full drop-in compatibility remains open for
the behaviors listed under deviations below.

## What was ported

| Upstream | Rust | Notes |
| --- | --- | --- |
| `src/bin.ts` | `src/cli.rs`, `src/main.rs` | `p3`, `start`, `server`, `status`, `stop`, `restart`, `list`, `--pi`, `--dir`, `--port`, `--help`, `--version`; usage is exit 2, a failed command exit 1 |
| `src/pi.ts` | `src/pi.rs` | `PiChoice`, `DEFAULT_PORT` 8789, `is_pi_name`, kept ports (`pi.json` with tab indentation), named Pis under `~/.pi/p3/pis/` |
| `src/files.ts` | `src/files.rs` | data directory 0700 with `~` and `/` refused, private directories/stores, one-step private writes (0600) |
| `src/records.ts` | `src/records.rs` | `running.json` and `supervisor-running.json` count only while their lock is held |
| `src/app.ts`, `src/principals.ts` | `src/app.rs`, `src/principals.rs` | API package/versions, facet host packages, the four principals |
| `src/install.ts` | `src/install.rs` | the worker/supervisor command of this binary, its version, plugins directory |
| `src/server-core.ts`, `src/worker-core.ts` | `src/server_core.rs`, `src/worker_core.rs` | base facets serving `ServerRuntime`, `WorkerRuntime` |
| `src/server.ts` | `src/server.rs` | data directory lock, host, Chord HTTP component (`/_p3` routes, `/chord` links), listening, `running.json`, close |
| `src/worker.ts` | `src/worker.rs` | worker bootstrap: start message, private directory, host, ready, reload, idle close, shutdown |
| `src/supervisor.ts` | `src/supervisor.rs` | one supervisor per data directory, crash restart with 1 s to 30 s backoff, clean stop, `logs/server.log` |
| `src/api/v1/{index,control,contribute,forward,server,worker}.ts` | `src/api/v1/`, `src/registry.rs` | host addresses, principals, control messages (`{ "p3": ... }` envelopes), `contribute`, `forward`, the typed registries |
| `plugins/control/` | `src/plugins/control.rs` | `POST /_p3/stop`, `POST /_p3/apply` (owner only, exact answer shape) |
| `plugins/runner/` | `src/runner.rs`, `src/plugins/runner_server.rs`, `src/plugins/runner_worker.rs` | one worker process per session, one operation per session, ready timeout, crash stickiness/retry, idle self-stop, rollout, clean close |
| `plugins/sessions/` | `src/plugins/sessions.rs` | rows, create/rename/archive/restore/report, active/archived ordered reads, 50-row pagination, owner/worker/server access checks, the waker for `working` sessions, route existence and admission |
| `plugins/sign-in/` | `src/plugins/sign_in.rs` | sign-ins with SHA-256 token hashes compared in constant time, 400-day idle expiry, signed rolling cookie, authenticator, owner link (single use, 10 minutes), owner token file (0600) |

`src/modules-server.ts` / `src/modules-worker.ts` have no counterpart: plugins are compiled in as
native Rust facets (ADR-002), so there are no per-process module tables.

`minimum-node.ts` becomes nothing: the binary has no guest runtime version to check.

## Wiring onto `pi-rs-chord`

- `node::http`: the whole HTTP component. `/chord` links are accepted through `Http::listen` and
  `HttpOptions::opened` (which is how sign-in hooks accepted links); routes, authenticators,
  wraps, cookies, provenance, nonce, CSP, and origin admission are Chord's, not a re-implementation.
- `node::app`: private directories and stores (0700/0600), and one-step secret writes.
- `node::workers::ProcessLock`: the server's and supervisor's per-data-directory locks.
- `host`: `create_host`, `load`/`reload` for the server and each worker, and the facet lifecycle.
- `pi-rs-chord` has no address-level worker routing yet (the `2.1b-3` gap). `p3` keeps that seam in
  three places, documented here so it can be replaced rather than worked around:
  - `server::SessionRouter`: does the session exist, who may reach it, which runner starts it.
  - `runner::WorkerRunner` / `runner::WorkerLauncher` / `runner::WorkerTransport`: the run loop and
    the process launch, so a Chord-routed worker can replace the local one.
  - `plugins::sessions::SessionFactsLink`: the worker side of `SessionFacts`/`SessionStart`.

## Deviations and honest limits

1. **Sessions and sign-ins on disk.** Upstream keeps the sessions table in the server's Chord store
   and the sign-ins as Chord documents. Here they are private JSON documents (`sessions.json`,
   `sign-ins.json`) written atomically (0600). The native table/document facets need a served local
   singleton contract, which the address-routing seam does not provide yet. The p3 SQLite schema is
   therefore not reproduced in 3.1.
2. **No Chord contracts are served.** `Sessions`, `SessionsList`, `SessionFacts`, `SessionWorker`,
   `SessionStart`, and `SignIns` exist as typed Rust entry points (`SessionsApi`, `SignIns`) with the
   same access checks, but an owner client cannot hold them over `/chord` yet. Authenticated `/chord`
   links are accepted and closed; they serve no p3 objects until the route exists.
3. **Worker links.** Control messages travel as newline-delimited JSON envelopes (`{"p3": ...}`) on
   the worker's stdio. Upstream also carries the object link frames on that channel; here the channel
   is control-only, because the route that would carry those frames is the `2.1b-3` gap above.
4. **Apply and reload.** Native facets are compiled into the binary. `apply` reloads the same facet
   identities, so no plugin is rebuilt; the workers' script set is empty and their rollout reaches
   nobody. The route and its answer shape (`steps`, `stopped`) are exact. A reload on a worker re-runs
   its facets and reports no failures.
5. Revoking a sign-in closes all of its open Chord links.
6. Unix workers start in separate process groups. Forced shutdown signals the group and falls back to the worker if the group has gone.
7. **Data-directory ownership** is checked by writability (and refusal of `~` and `/`), not by
   `getuid`: no safe standard API exposes it.
8. **Worker process exit.** After its host closes, a worker exits with `process::exit`, as upstream's
   forced-exit path does; a runtime with leftover tasks must not keep it alive.
9. **Version** reports the pinned upstream p3 package version (`0.0.1`).
10. **Supervised detection** is `P3_SUPERVISED=1` in the environment, where upstream uses
    `process.send` being defined.
11. **Hidden modes.** `p3 __worker` and `p3 __supervisor <data dir> <argv...>` are internal modes of
    the one binary; they are not in the usage text.

## Acceptance gates

All commands use the worktree manifest and `/var/tmp/pi-rs-target-oc-ds` with
`CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0` and `--offline -j 2`.

| Gate | Result | Evidence |
| --- | --- | --- |
| Formatting | PASS | `cargo fmt -p pi-rs-p3 -- --check` |
| Warning-free lint | PASS | `cargo clippy -p pi-rs-p3 --all-targets -- -D warnings` |
| Offline test suite | PASS | `cargo test -p pi-rs-p3`: 79/79 (69 library, 6 CLI binary, 4 runner) |
| Real `p3` binary: help/version, refused lines, stopped status, empty list | PASS | `tests/cli.rs` |
| Real `p3` binary: background start, port, `pi.json`, owner link, sign-in cookie, fresh link, stop | PASS | `tests/cli.rs`, on a temporary `--dir` with `--port 0` |
| Real `p3` binary: foreground start, status, owner stop, clean exit | PASS | `tests/cli.rs` |
| Named Pi keeps its port; `list` and `restart` | PASS | `tests/cli.rs` |
| Worker as a real child process: start/ready, clean stop, idle self-stop and restart | PASS | `tests/runner.rs` |
| Worker crash fails its session until retry | PASS | `tests/runner.rs` (transport killed) |
| Rollout reaches a running worker and clears failures | PASS | `tests/runner.rs` |
| Supervisor restarts a crashed server; stops its child cleanly | PASS | `src/supervisor.rs` tests |
| Data directory/private files/records/ports/principals/control/forward/contribute | PASS | library tests |
| Sessions store: order, archive/restore, pagination, persistence, access, waker, admission | PASS | library tests |
| Sign-ins: issue/verify/revoke, constant-time match, persistence, owner token file, one-time link, rolling cookie surface | PASS | library tests |
| Mutation checks | PASS | 5 of 5 broke their test (see below) |
| No `unsafe`, no `#[allow]`, no `unwrap`/`expect`/`panic!` on fallible paths | PASS | `#![forbid(unsafe_code)]`; only the documented INFALLIBLE epoch fallback uses `expect` |

### Mutation checks

Each fault was applied to the source, the covering test observed failing, and the file restored.
SHA-256 before and after matched for every file (`/tmp/opencode/p3-mutation-final.txt`).

| Fault | Test that failed |
| --- | --- |
| A Pi's name accepts upper case | `pi::tests::a_pi_name_is_lower_case_and_bounded` |
| An owner link stays usable after its first use | `plugins::sign_in::tests::an_owner_link_is_used_up_by_its_first_consumption` |
| A session's worker may reach another session | `plugins::sessions::tests::admission_allows_a_session_that_exists_and_never_another_sessions_worker` |
| Every worker exit counts as clean | `tests/runner.rs::a_crashed_worker_fails_its_session_until_retry` |
| The supervisor never restarts a crashed server | `supervisor::tests::a_crashed_server_starts_again` |

### Tests added

79 tests, none removed. Library suite: 69 tests in 1.46 s. Binary integration: 6 tests in 0.28 s.
Runner integration: 4 tests in 0.15 s. The suite runs offline and starts real `p3` processes; no
network, no npm, no Node.

## Next

The next unit that would lift the largest fidelity limit is the `2.1b-3` address-level route in
`pi-rs-chord`: with it, the sessions and sign-in contracts can be served over `/chord`, the worker
control channel can carry link frames beside control messages, and the JSON stores can move to Chord
documents without changing p3's external behavior.
