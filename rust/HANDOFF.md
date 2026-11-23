# pi-rs coordinator handoff

**Stopped on 2026-11-23.** All six Astra Low and both Sol Medium Herdr workers exited. Their builds ended. All checkpoints are local. Upstream remains `earendil-works/pi` v1.6.0 at `780bf31fc`.

**Resume from `checkpoint/integration-2026-11-23` at `42621514e`.** It contains the combined hosts, Chord changes, worker environment, TUI wheel and theme support, and the latest durable lifecycle changes composed with native telemetry. The primary checkout is on `main`. Its last code change is `0b4979219`. Subsequent changes on main at shutdown are documentation only.

## Verification at shutdown

| Area | Status and evidence |
| --- | --- |
| Workspace | The earlier 17-crate checkpoint passed 2,236 native tests and 21 leaf proof gates. The workspace now has 18 members. The combined 18-crate fmt, Clippy, WASM and native test gates are outstanding. |
| Durable harness | On main at `bb908bd1c`, all 91 tests passed and strict all-target Clippy passed. The later worker candidate passed 92 tests on its own base and 81 upstream cases across five lifecycle suites. It adds six tests and strengthens two. The integrated revision has not run this suite. |
| Durable environment bridge | 46 native tests passed. Two recorded live daemon scenarios matched upstream byte for byte. Both failed under mutation. The proof is in `rust/verification/env-listing`. |
| Native p2 and browser | On main at `0b4979219`, all ten native tests passed with the production browser bundle. Two mutation checks cover owner access and stale-page reload. Six core HTTP replays passed earlier. The 106 shared contracts match 130 upstream versions. End-to-end native session creation, chat, and persistence are open. |
| Worker placement and session documents | The placement candidate passed two native tests, the reader-precedence and confinement mutations, two upstream location cases and strict Clippy. Live SSH remains untested. The session-document candidate passed its reopen test, both mutations, strict Clippy and 14 upstream tree cases. |
| Terminal | The launcher candidate passed the guest-panel PTY exercise and six HTTP replays. The upstream TUI run had 130 passes, two failures and one cancellation out of 133. Two affected cases passed on retry. Clipboard still timed out. The checkpointed WASM fixture has eight embedded build paths anonymized and was validated with the component encoder. The PTY run preceded that metadata change. |
| Combined host integration | **Clippy failed** at `pi-rs-chord/src/host.rs:238` with E0308. `map_or` inferred a function-item type for the default and received a function pointer from the policy. The affected test suite and the final whole-workspace gates have not passed on this candidate. |
| Built-in plugins | The six Wave 2 briefs cover all 49 plugin directories. No plugin-shard worktrees were launched. Complete the host contracts and native session/chat path before counting whole-app parity. |

## Local recovery branches

Each worker checkout is under `.worktrees/<branch suffix>`. These branches overlap. Several include dependencies copied from a different worker's branch for compilation. Start from the combined checkpoint and integrate the outstanding changes selectively.

| Branch | Commit | Contents and remaining work |
| --- | --- | --- |
| `wt/durable-2.2b` | `9b91a7726` | Standalone harness, 25-module export audit and lifecycle verification. The combined checkpoint includes its later changes alongside observation and telemetry. |
| `wt/p2-revocation` | `0aaf0008d` | Earlier composition of revocation, server naming, logs, routing, dev socket and terminal launcher. Superseded as the integration starting point by `42621514e`. |
| `wt/p2-routing` | `bbebf74e7` | Supervised session workers, caller forwarding, idle restart and authorization. Incorporated into the combined candidate. |
| `wt/p2-dev-control` | `4e07d9d0f` | Standalone development controls. Incorporated into the combined candidate. |
| `wt/p2-runtime-logs` | `ba710e152` | Process logs, redaction and host lifecycle. Incorporated into the combined candidate. |
| `wt/p2-worker-environment` | `d4008d43d` | Typed location, placement, confinement and environment lifecycle. Incorporated into the combined candidate. |
| `wt/p2-launcher` | `c380592a3` | The additional guest panel and shortcut dispatch work still needs integration. |
| `wt/p2-plugin-discovery` | `b469a2b63` | Native and component discovery. Complete and verify production server and worker refresh. The tested discovery patch and a separate temporary integration overlay are in the evidence directory. |
| `wt/p2-session-documents` | `875d5d52d` | Session store, main-conversation migration and session locks. Integrate with the worker harness. |
| `wt/p2-session-files` | `11e01885d` | File transfer and filename/preview handling. Upstream filename cases ran. Native tests, mutations, Clippy and Unicode boundary parity are open. |
| `wt/p2-server-registries` | `682fef73a` | Server services, request access and supervised host control. Four tests passed against cached artifacts on an earlier revision. The final Cargo and mutation run was interrupted. |
| `wt/p2-worker-harness` | `a81d90907` | Native worker core, durable session startup and Chord's pre-ready hook. Its cold build was interrupted. |
| `wt/durable-harness-proof` | `0b4979219` | Empty preparation worktree. No differential harness proof was recorded. |

## Resume sequence

1. Start an isolated checkout with `git worktree add .worktrees/integration-resume checkpoint/integration-2026-11-23`. Fix the Chord type error there. Run strict offline all-target Clippy on `pi-rs-chord`, `pi-rs-p2` and `pi-rs-p2-tui`. Exercise revocation, server rename/restart, default routing, stale-browser handling and the standalone terminal.
2. Run the combined durable suite and close three audited host contracts. Invalid structured tool output must report the owning extension. A failing host clock must fall back and report once, including reentrant reporting. A host-supplied model selector must be read at use time. Upstream reference cases are in `harness-structured-output.test.ts`, `harness-storage-failure.test.ts` and `harness-host-inputs.test.ts`.
3. Compose session documents, session files, plugin discovery, server registries, the worker harness and the remaining launcher changes. Exercise a real child reaching Ready only after its durable session opens. Then distribute the 49 plugins and verify the native binary with a disposable data directory, authenticated owner and guest, fake model, persistence across restart, HTTP, WebSocket, terminal, and browser.
4. Run the full 18-crate workspace format, offline strict Clippy, WASM check and native tests. Confirm `git diff 780bf31fc -- packages/ apps/` is empty and check privacy before delivery.

The local evidence directory is `.git/pi-rs-wind-down-2026-11-23/`. It contains the worker briefs, selected gate and mutation logs, the frozen panel and discovery patches, and the discovery overlay. Build caches under `/var/tmp/pi-rs-target-*` were retained. Space was limited at shutdown. Use per-worker `TMPDIR` and `CARGO_TARGET_DIR` under `/var/tmp`, debug profiles set to zero and `--offline -j1` for cold verification. Keep upstream source read-only and commits local. See [MAP.md](MAP.md) for the milestone inventory and [DECISIONS.md](DECISIONS.md) for the binding ADRs.
