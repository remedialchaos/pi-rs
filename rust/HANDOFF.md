# `pi-rs` Handoff

- **Commit**: Uncommitted working tree on top of `591137fa2` (base `780bf31fc`, `packages/` and `apps/` untouched; held unpushed per user)
- **Stage**: **PAUSED (All Workers Cleared & Idle) — Phase 1 100% Complete, 13 Verified Crates in Unified `rust/crates/Cargo.toml` Workspace, 21 Leaf Proof Gates, 1,902 Verified Native Tests (~61.0% Overall LOC Verified)**
- **Fallback rate**: `pi` binary runs 100% natively (interactive main/alt TUI, print, JSON, RPC, auth, packages); `packages/` and `apps/` untouched.
- **Next action**: Paused per user request to address Open Questions (#1 `apps/p3/old/`, #2 WASM UI `leptos` confirmation) and receive user's additional instruction before starting the next wave (`2.1b` orchestration gap closure vs `2.2` `pi-rs-durable` -> `2.3b` `pi-rs-env` bridge -> Phase 3 `p3`/`p2`).

## Gate Table

| Gate | Status | Evidence |
| --- | --- | --- |
| Alignment, `doctor`, `analyze`, repo docs/license, oracle hydration | PASS | ADR-001..010 in [DECISIONS.md](DECISIONS.md); `git diff packages/ apps/` empty; repo-wide privacy clean |
| All primary-source research (8 of 8 topics) | PASS | All 8 notes complete in `rust/research/` |
| Unified 13-crate Cargo workspace (`rust/crates/Cargo.toml`) | **PASS** | `cargo fmt --all --check` PASS; all modified crates pass offline `clippy --all-targets -- -D warnings` and `cargo test` |
| **1.1** `pi-rs-schema` | **PASS** | 2/2 leaf gates (2,061/2,061); 118/118 tests, 0 unsafe |
| **1.2** `pi-rs-telemetry` | **PASS** | 1/1 leaf gate (210/210, 100% cov); 56/56 tests (incl. `TelemetryRecorderSink` & `FallibleTelemetryStore`), 0 unsafe |
| **1.3** `pi-rs-mcp` | **PASS** | 3/3 leaf gates (870/870); 142/142 tests, 0 lib unsafe |
| **1.4** `pi-rs-codemode` | **PASS** | 3/3 leaf gates (1,174/1,174); ADR-010 `wasmtime 50.0.2` + `quickjs.wasm 3.6.2`, 273/273 tests, `#![forbid(unsafe_code)]` |
| **1.5** `pi-rs-tui` | **PASS** | 5/5 leaf gates (7,782/7,782); 96/96 tests |
| **1.6** `pi-rs-ai` | **PASS** | 2/2 leaf gates (885/885); 39/39 offline loopback tests (`pi-ai` bin), 0 unsafe |
| **1.7** `pi-rs-agent` | **PASS** | 32/32 offline tests, 0 unsafe/allow/unwrap |
| **1.8** `pi-rs-coding-agent` (`1.8a` + `1.8b-1` + `1.8b-2a` + `1.8b-2b`, `pi` bin) | **PASS (Phase 1 100%)** | 3/3 leaf gates (1,839/1,839); **865/865** offline + real Linux PTY tests + 8/8 TS oracle replay + loopback print/15-event JSON parity + WIT WASM test, `#![forbid(unsafe_code)]`, 0 `#[allow]` |
| **2.1a + 2.1b-1 + 2.1b-2** `pi-rs-chord` foundation + portable + platform checkpoint | **PASS (bounded)** | 2/2 leaf gates (4,995/4,995); **163/163** tests (19 wire goldens, telemetry bridge, live Axum HTTP/WS RPC, `wit/plugin.wit`, real child workers, auth/files/archive), `#![forbid(unsafe_code)]`; remaining platform orchestration gaps noted below |
| **2.2** `pi-rs-durable` | PENDING | 35.9k LOC; unblocked once user confirms next wave |
| **2.3a** `pi-rs-env` daemon (`pi-env` bin) + client transport | **PASS** | **41/41** tests (33 unit + 8 live `pi-env` binary integration), `unsafe` isolated to `daemon/sys/`, 0 `#[allow]` |
| **2.3b** `pi-rs-env` durable `ExecutionEnv` bridge | PENDING | Follows `2.2` |
| **2.4** `pi-rs-evals` (`pi-evals` bin) | **PASS** | **44/44** tests in 0.02s (5/5 mutation checks, golden report/SHA parity), `#![forbid(unsafe_code)]`, 0 `#[allow]` |
| **3.1–3.2** Phase 3 remaining (`p3`, `p2` + Leptos WASM web UI) | PENDING | Follow Phase 2 + Open Questions #1 & #2 |
| **3.3** `pi-rs-p2-mac` | **PASS** | 16/16 Linux tests + `aarch64-apple-darwin` check + real upstream `p2` smoke PASS |
| **3.4** `pi-rs-voice-server` | **PASS** | 17/17 offline tests + real Supertonic/Kokoro/Silero/Parakeet verified |

## Key Links

- Plan & unit claims: [MAP.md](MAP.md) | Decisions: [DECISIONS.md](DECISIONS.md) | Glossary: [GLOSSARY.md](GLOSSARY.md) | Leaf map: [PORT_MAP.md](PORT_MAP.md) | Workspace: [crates/Cargo.toml](crates/Cargo.toml)

## Known Hazards & Notes

1. **Workspace layout**: Cargo workspace root MUST stay at `rust/crates/Cargo.toml` so local `rust/pi-rs-*` proof crates stay outside Cargo discovery and ignored via `/pi-rs-*/` in `rust/.gitignore`.
2. **Build cache disk**: `/tmp` is a 4 GiB tmpfs; always point concurrent worker `CARGO_TARGET_DIR` to `/var/tmp/pi-rs-target-<worker>` and clean it after verification.
3. **Documented fidelity limits** (recorded per crate `MILESTONE.md`):
   - `pi-rs-coding-agent`: standalone `pi mcp` subcommand, MCP OAuth/tool exposure, WASM guest UI dialogs, startup catalog refresh/auto-install, and exact interactive pixel replay remain future polish.
   - `pi-rs-chord`: 2.1b-2 is a bounded checkpoint — `HostData` address ownership/worker routing, registry-facing `Files`/`Http` adapters, streaming format-2 app archives, and full browser/launcher bootstrap remain open (see `pi-rs-chord/MILESTONE.md` lines 304–335).
   - `pi-rs-env`: `packaged_daemon()` assumes npm `bin/` layout (`local_daemon()` is the native path); live `sshd`/Windows/macOS runtime untested on Linux host.
   - `pi-rs-evals`: offline Docker verification via injectable runner; no live Docker daemon run.
   - `pi-rs-p2-mac` & `pi-rs-voice-server`: macOS UI/signing deferred per ADR-005; Pocket TTS is a discovery/streaming hook.
4. **Git & privacy safety**: `packages/` and `apps/` untouched; no personal real-name metadata; uncommitted wave changes ready to commit when user directs.
