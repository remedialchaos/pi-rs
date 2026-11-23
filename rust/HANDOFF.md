# `pi-rs` Handoff

- **Base commit**: `1e6e4aa16` on local `main`, three commits ahead of `origin/main`.
- **Stage**: Wave 1 in progress. The previous checkpoint recorded 17 workspace crates, 21 leaf gates and 2,236 native tests.
- **Fallback rate**: `pi`, `pi-ai`, `pi-env`, `pi-evals`, `p3`, `pi-mac`, and `pi-voice-server` binaries run 100% natively; `packages/` and `apps/` untouched.
- **Next action**: Verify and integrate the Wave 1 claims in [MAP.md](MAP.md) before assigning the 49 built-in plugins. Isolated worktrees under `.worktrees/` contain unmerged work.

## Gate Table

| Gate | Status | Evidence |
| --- | --- | --- |
| Alignment, `doctor`, `analyze`, repo docs/license, oracle hydration | PASS | ADR-001..011 in [DECISIONS.md](DECISIONS.md); `git diff packages/ apps/` empty; repo-wide privacy clean |
| All primary-source research (8 of 8 topics) | PASS | All 8 notes complete in `rust/research/` |
| Unified 17-crate Cargo workspace (`rust/crates/Cargo.toml`) | **PASS** | `cargo fmt --all --check` PASS; offline `cargo clippy --workspace --all-targets -- -D warnings` PASS; `wasm32-unknown-unknown` check PASS |
| **1.1** `pi-rs-schema` | **PASS** | 2/2 leaf gates (2,061/2,061); 118/118 tests, 0 unsafe |
| **1.2** `pi-rs-telemetry` | **PASS** | 1/1 leaf gate (210/210, 100% cov); 56/56 tests, 0 unsafe |
| **1.3** `pi-rs-mcp` | **PASS** | 3/3 leaf gates (870/870); 142/142 tests, 0 lib unsafe |
| **1.4** `pi-rs-codemode` | **PASS** | 3/3 leaf gates (1,174/1,174); ADR-010 `wasmtime 50.0.2` + `quickjs.wasm 3.6.2`, 273/273 tests, `#![forbid(unsafe_code)]` |
| **1.5** `pi-rs-tui` | **PASS** | 5/5 leaf gates (7,782/7,782); 96/96 tests |
| **1.6** `pi-rs-ai` | **PASS** | 2/2 leaf gates (885/885); 39/39 offline loopback tests (`pi-ai` bin), 0 unsafe |
| **1.7** `pi-rs-agent` | **PASS** | 32/32 offline tests, 0 unsafe/allow/unwrap |
| **1.8** `pi-rs-coding-agent` (`1.8a`–`1.8c`, `pi` bin) | **PASS (Phase 1 100% + Parity)** | 3/3 leaf gates (1,839/1,839); **879/879** offline + PTY + `pi mcp` CLI + live MCP/OAuth loopback + expanded WIT UI/registrations + provider/virtual-model reload, `#![forbid(unsafe_code)]`, 0 `#[allow]` |
| **2.1** `pi-rs-chord` (`2.1a` + `2.1b-1`..`2.1b-3`) | **PASS** | 2/2 leaf gates (4,995/4,995); **178/178** tests (19 wire goldens, Axum HTTP/WS RPC, real checked-in WASM guest components, `HostData` owner transfer, worker routing, registry-bound `Http`/`Files`/`Users`, TUI launcher), `#![forbid(unsafe_code)]` |
| **2.2a** `pi-rs-durable` foundation, env, images, tools, codemode, storage & session | **PASS** | **77/77** contract tests (memory/SQLite/Chord-store/JSONL + local `ExecutionEnv` conformance, 16 mutation checks), `#![forbid(unsafe_code)]`, 0 `#[allow]` |
| **2.2b** `pi-rs-durable` harness (`src/harness/`) | PENDING | ~12.5k LOC (25 files); in progress |
| **2.3a** `pi-rs-env` daemon (`pi-env` bin) + client transport | **PASS** | **41/41** tests (33 unit + 8 live `pi-env` binary integration; `RemoteError.fields` boxed for workspace `preserve_order`), `unsafe` isolated to `daemon/sys/`, 0 `#[allow]` |
| **2.3b** `pi-rs-env` durable `ExecutionEnv` bridge | PENDING | ~1.1k LOC (2 units); uses the `ExecutionEnv` contract from 2.2a |
| **2.4** `pi-rs-evals` (`pi-evals` bin) | **PASS** | **44/44** tests (5/5 mutation checks, golden report/SHA parity, TTY-deterministic styling), `#![forbid(unsafe_code)]`, 0 `#[allow]` |
| **3.1** `pi-rs-p3` (`p3` bin; `old/` skipped) | **PASS** | **79/79** tests (69 lib + 6 live `p3` binary CLI + 4 live child-worker runner, 5/5 mutation checks), `#![forbid(unsafe_code)]`, 0 `#[allow]` |
| **3.2-web** Browser | PARTIAL | 135 native tests. Compiled WASM created a session and exchanged chat messages with the upstream server in Chromium. The fixture injected assets with CSP bypass. Production browser routing, CSP and plugin integration remain open. |
| **3.2-tui** `pi-rs-p2-tui` (`p2` TUI client on `pi-rs-tui`) | **PASS (bounded)** | **15/15** tests (5 unit + 10 integration, 15/15 mutation checks: native Mermaid, masked input, shell status, settings, slash completion, palettes, transcript anchors), `#![forbid(unsafe_code)]`, 0 `#[allow]` |
| **3.2** `pi-rs-p2` server + runtime + 49 plugins (`p2` bin) | PENDING | ~89.6k LOC; uses the `ExecutionEnv` contract from 2.2a + `2.3b` |
| **3.3** `pi-rs-p2-mac` (`pi-mac` bin) | **PASS** | 16/16 Linux tests + `aarch64-apple-darwin` check + real upstream `p2` smoke PASS |
| **3.4** `pi-rs-voice-server` (`pi-voice-server` bin) | **PASS** | 17/17 offline tests + real Supertonic/Kokoro/Silero/Parakeet verified |

The upstream server and the standalone Rust Wave 1 binary (SHA-256 `4f186820268344cec5bfd99229649d9228730fe9002d1cce0f7af129064d1b49`) each passed the six [HTTP cases](app-cases/http/). Run with `P2_FAKE=1` and a disposable `P2_DATA_DIR`. The upstream WebSocket session creation, retry and chat case passed in `apps/p2/test/p2/session-operations.test.ts`. Native authenticated session/chat flow and plugin integration are pending.

## Key Links

- Plan & claims: [MAP.md](MAP.md) | Decisions: [DECISIONS.md](DECISIONS.md) | Glossary: [GLOSSARY.md](GLOSSARY.md) | Leaf map: [PORT_MAP.md](PORT_MAP.md) | Workspace: [crates/Cargo.toml](crates/Cargo.toml)

## Known Hazards & Notes

1. **Workspace layout**: Cargo workspace root MUST stay at `rust/crates/Cargo.toml` so local `rust/pi-rs-*` proof crates stay outside Cargo discovery and ignored via `/pi-rs-*/` in `rust/.gitignore`.
2. **Build cache & temp disk**: `/tmp` is a 4 GiB tmpfs. For concurrent worktrees, always set both `CARGO_TARGET_DIR=/var/tmp/pi-rs-target-<worker>` and `TMPDIR=/var/tmp/pi-rs-tmp-<worker>` with `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0`, and clean both after merging.
3. **Documented fidelity limits** (recorded per crate `MILESTONE.md`):
   - `pi-rs-coding-agent`: external live OAuth browser sign-in, startup background catalog refresh/auto-install, named single-server MCP reconnect (currently rebuilds all session MCP connections), and exact interactive pixel replay remain future polish.
   - `pi-rs-p2-tui`: bounded checkpoint — incremental transcript lifecycle, panel scrolling, tree layout, plugin dispatch, and live theme reload parity remain open (see `pi-rs-p2-tui/MILESTONE.md`).
   - `pi-rs-env`: `packaged_daemon()` assumes npm `bin/` layout (`local_daemon()` is the native path); live `sshd`/Windows/macOS runtime untested on Linux host.
   - `pi-rs-evals`: offline Docker verification via injectable runner; no live Docker daemon run.
   - `pi-rs-p2-mac` & `pi-rs-voice-server`: macOS UI/signing deferred per ADR-005; Pocket TTS is a discovery/streaming hook.
4. **Git & privacy safety**: `packages/` and `apps/` untouched; zero personal real-name metadata or home paths; local commits held unpushed until user directs.
