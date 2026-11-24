# `pi-rs` Handoff

- **Checkpoint**: Wave 2 + Phase 3 integration + Phase 4 E2E complete (`33f1ab38d` + docs commit, merged locally onto `main`; **not pushed**).
- **Stage**: All 18 workspace crates, all 8 binaries (`pi`, `pi-ai`, `pi-env`, `pi-evals`, `p3`, `p2`, `pi-mac`, `pi-voice-server`), and all 49 built-in `p2` plugin directories (48 facet-bearing plugins + `sync` library module) are ported, integrated, and passing offline workspace gates.
- **Fallback rate**: 0% fallback; `packages/` and `apps/` remain untouched (`git diff 780bf31fc -- packages/ apps/` is empty).
- **Next action**: User review/push decision on local `main`, plus optional follow-up polish (live Chromium browser E2E with all 48 plugins, dynamic TUI guest dispatch, and any crate-level fidelity items below).

## Gate Table

| Gate | Status | Evidence |
| --- | --- | --- |
| Alignment, `doctor`, `analyze`, repo docs/license, oracle hydration | **PASS** | ADR-001..011 in [DECISIONS.md](DECISIONS.md); `git diff 780bf31fc -- packages/ apps/` empty; privacy scan 0 matches |
| Primary-source research (8 of 8 topics) | **PASS** | All 8 notes complete in `rust/research/` |
| Unified 18-crate Cargo workspace (`rust/crates/Cargo.toml`) | **PASS** | `cargo fmt --check` PASS; offline `cargo clippy --workspace --all-targets -- -D warnings` PASS (0 warnings); `cargo check --target wasm32-unknown-unknown -p pi-rs-p2-web` PASS |
| **1.1** `pi-rs-schema` | **PASS** | 2/2 leaf gates (2,061/2,061); workspace suite PASS, 0 unsafe |
| **1.2** `pi-rs-telemetry` | **PASS** | 1/1 leaf gate (210/210, 100% cov); workspace suite + 2/2 compile-fail doctests PASS, 0 unsafe |
| **1.3** `pi-rs-mcp` | **PASS** | 3/3 leaf gates (870/870); workspace suite PASS, 0 lib unsafe |
| **1.4** `pi-rs-codemode` | **PASS** | 3/3 leaf gates (1,174/1,174); `wasmtime 50.0.2` + `quickjs.wasm 3.6.2`, workspace suite PASS, `#![forbid(unsafe_code)]` |
| **1.5** `pi-rs-tui` | **PASS** | 5/5 leaf gates (7,782/7,782); workspace suite PASS |
| **1.6** `pi-rs-ai` (`pi-ai` bin) | **PASS** | 2/2 leaf gates (885/885); 39/39 offline loopback tests PASS, 0 unsafe |
| **1.7** `pi-rs-agent` | **PASS** | 32/32 offline tests PASS, 0 unsafe/allow/unwrap |
| **1.8** `pi-rs-coding-agent` (`pi` bin) | **PASS** | 3/3 leaf gates (1,839/1,839); workspace suite PASS (CLI/print/JSON/RPC/PTY/`pi mcp`/MCP+OAuth/WIT), `#![forbid(unsafe_code)]` |
| **2.1** `pi-rs-chord` | **PASS** | 2/2 leaf gates (4,995/4,995); workspace suite PASS incl. `start_worker_with_setup` + exposed `WorkerApp::parent`, `#![forbid(unsafe_code)]` |
| **2.2** `pi-rs-durable` (`2.2a` + `2.2b`) | **PASS** | Storage/env/session + full harness restart/recovery/cancellation tests PASS, `#![forbid(unsafe_code)]` |
| **2.3** `pi-rs-env` (`pi-env` bin) | **PASS** | Daemon + durable bridge + in-process PTY transport for `p2` terminals; workspace suite PASS |
| **2.4** `pi-rs-evals` (`pi-evals` bin) | **PASS** | Workspace suite PASS (golden report/SHA parity), `#![forbid(unsafe_code)]` |
| **3.1** `pi-rs-p3` (`p3` bin) | **PASS** | Workspace suite PASS (lib + live `p3` CLI + live child-worker runner), `#![forbid(unsafe_code)]` |
| **3.2-web** `pi-rs-p2-web` | **PASS (core)** | 135/135 native tests + `wasm32-unknown-unknown` build PASS; packaged into `pi-rs-p2` under production CSP |
| **3.2-tui** `pi-rs-p2-tui` | **PASS (core)** | 22/22 native tests (5 unit + 11 headless + 6 rendering) + `pi-rs-p2` TUI launcher tests PASS |
| **3.2** `pi-rs-p2` (`p2` bin, all 49 plugins) | **PASS** | All 49 upstream plugins ported (48 facet plugins + `sync` library); Phase 3 cross-plugin seams wired (`p2.server-peer`, `p2.worker-files`, `p2.send-routers`, `p2.radius-forward-resolver`, `p2.backup-targets`, `p2.model-resources`, `p2.settings-saver`, `p2.terminal-env-provider`, `p2.telemetry-store`, `p2.plugin-reload-handler`, `copy_session_artifacts`, `p2.status-report` worker facet); all 25 test binaries PASS incl. live `p2_all_builtins_e2e` (HTTP, cross-shard services, real `p2 --worker` chat turn, shutdown + restart persistence) |
| **3.3** `pi-rs-p2-mac` (`pi-mac` bin) | **PASS** | 16/16 Linux tests PASS on `main` checkout + `aarch64-apple-darwin` type-check PASS |
| **3.4** `pi-rs-voice-server` (`pi-voice-server` bin) | **PASS** | 17/17 offline tests PASS + real Supertonic/Kokoro/Silero/Parakeet verified |

## Key Links

- Plan & claims: [MAP.md](MAP.md) | Decisions: [DECISIONS.md](DECISIONS.md) | Glossary: [GLOSSARY.md](GLOSSARY.md) | Leaf map: [PORT_MAP.md](PORT_MAP.md) | Workspace: [crates/Cargo.toml](crates/Cargo.toml)

## Known Hazards & Notes

1. **Workspace layout**: Cargo workspace root MUST stay at `rust/crates/Cargo.toml` so local `rust/pi-rs-*` proof crates stay outside Cargo discovery and ignored via `/pi-rs-*/` in `rust/.gitignore`.
2. **Build cache & temp disk**: `/tmp` is a 4 GiB tmpfs. For large builds, set `CARGO_TARGET_DIR=/var/tmp/pi-rs-target-...` with `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0`. Avoid setting long `TMPDIR` or deep worktree paths when running `pi-rs-p2-mac` Unix-socket tests (`SUN_LEN` 108-byte limit).
3. **`install_service` lifetimes**: `runtime/server_services.rs::install_service` stores `Weak<T>` keyed by service name string. Always retain a strong `Arc<T>` for the host/object lifetime and use distinct service keys per concrete type `T`.
4. **Worker idle shutdown**: Worker facets MUST NOT hold server contracts on the worker's own `Host` (`host.hold(...)`), which increments `host.activity().references` and prevents idle exit; use `"p2.server-peer"` (`RemotePeer::hold`) instead.
5. **Documented fidelity limits** (recorded per crate `MILESTONE.md`):
   - `pi-rs-coding-agent`: external live OAuth browser sign-in, startup background catalog refresh/auto-install, single-server MCP reconnect, and exact interactive pixel replay remain future polish.
   - `pi-rs-p2-web` / `pi-rs-p2-tui`: live Chromium browser E2E with all 48 built-in plugin UIs, dynamic TUI guest dispatch, and full upstream screen comparison remain follow-up polish.
   - `pi-rs-p2` `ports`: `SessionArchived` cleanup contribution awaits a keyed native-registry seam (archived-session sweep at open is active).
   - `pi-rs-env`: `packaged_daemon()` assumes npm `bin/` layout (`local_daemon()` is the native path); live `sshd`/Windows/macOS runtime untested on Linux host.
   - `pi-rs-evals`: offline Docker verification via injectable runner; no live Docker daemon run.
   - `pi-rs-p2-mac` & `pi-rs-voice-server`: macOS UI/signing deferred per ADR-005; Pocket TTS is a discovery/streaming hook.
6. **Git & privacy safety**: `packages/` and `apps/` untouched; zero personal real-name metadata or home paths; local `main` held unpushed until user directs.
