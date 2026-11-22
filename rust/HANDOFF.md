# `pi-rs` Handoff

- **Commit**: Unified 11-crate workspace checkpoint on `main` (base `780bf31fc`, `packages/` and `apps/` untouched)
- **Stage**: **PAUSED (All 3 Worker Panes Idle) — 11 Rust Crates in Unified `rust/crates/Cargo.toml` Workspace, 21 Leaf Proof Gates, 1,655 Verified Native Tests**
- **Fallback rate**: N/A (front doors not scaffolded yet; `packages/` and `apps/` untouched)
- **Next action**: Waiting on user direction to resume dispatching. Immediate unblocked priorities:
  1. **Milestone 1.8b-2** (`cli/`, `modes/{print,json,rpc,interactive}/`, `pi` binary + whole-app `rust-anything app record/replay`) -> **completes Phase 1**.
  2. **Milestone 2.1b** (`pi-rs-chord` host/jobs/HTTP/workers/WASM plugins/surfaces/TUI/app) -> unlocks **2.2 `pi-rs-durable`**, **2.3 `pi-rs-env`**, **2.4 `pi-rs-evals`**, and Phase 3 apps **3.1 `p3`** & **3.2 `p2`**.

## Gate Table

| Gate | Status | Evidence |
| --- | --- | --- |
| Alignment, `doctor`, `analyze`, repo docs/license, oracle hydration | PASS | ADR-001..010 in [DECISIONS.md](DECISIONS.md); `git diff packages/ apps/` empty; repo-wide privacy clean |
| All primary-source research (8 of 8 topics) | PASS | All 8 notes complete in `rust/research/` |
| Unified 11-crate Cargo workspace (`rust/crates/Cargo.toml`) | **PASS** | `cargo fmt --all --check` & `cargo clippy --workspace --all-targets -- -D warnings` PASS; all 4 integration items resolved (SunPro `cbrt` inlined in `pi-rs-tui`, additive `pi-rs-telemetry` bridge landed, `wit/extension.wit` + `echo_extension.wasm` verified, `pi-rs-codemode` leaf sources/fixtures inlined) |
| **1.1** `pi-rs-schema` | **PASS** | 2/2 leaf gates (2,061/2,061); 118/118 tests (4,213/4,213 JSON Schema suite + 204 TypeBox convert), 0 unsafe |
| **1.2** `pi-rs-telemetry` | **PASS** | 1/1 leaf gate (210/210, 100% cov); 56/56 tests (54 + 2 doctests, incl. `TelemetryRecorderSink` & `FallibleTelemetryStore` bridge), 0 unsafe |
| **1.3** `pi-rs-mcp` | **PASS** | 3/3 leaf gates (870/870); 142/142 tests (141 + 1 doctest), 0 lib unsafe |
| **1.4** `pi-rs-codemode` | **PASS** | 3/3 leaf gates (1,174/1,174 inlined into crate); ADR-010 `wasmtime 50.0.2` + `quickjs.wasm 3.6.2`, 273/273 tests (24/24 inlined-leaf/lib/decl re-verified in unified workspace), `#![forbid(unsafe_code)]` |
| **1.5** `pi-rs-tui` | **PASS** | 5/5 leaf gates (7,782/7,782; `pi-rs-tui-oklab` re-verified 1,129/1,129 after inlining SunPro `cbrt`); 96/96 tests |
| **1.6** `pi-rs-ai` | **PASS** | 2/2 leaf gates (885/885); 39/39 offline loopback tests (10 chat + 1 image + 4 classifier APIs + 9 OAuth + `pi-ai`), 0 unsafe |
| **1.7** `pi-rs-agent` | **PASS** | 32/32 offline tests (faux registry + loopback SSE), 0 unsafe/allow/unwrap |
| **1.8a + 1.8b-1** `pi-rs-coding-agent` foundation + core runtime | **PASS** | 3/3 leaf gates (1,839/1,839); 759/759 offline tests (incl. `pi-rs-tui` wired directly and `wit/extension.wit` + `echo_extension.wasm` component test), 0 unsafe/allow |
| **1.8b-2** `pi-rs-coding-agent` modes & `pi` CLI | PENDING | Final milestone of Phase 1 |
| **2.1a** `pi-rs-chord` foundation + telemetry bridge | **PASS** | 2/2 leaf gates (4,995/4,995); 107/107 offline tests incl. all 19 untouched-TS binary & text goldens + `tests/telemetry_bridge.rs`, `#![forbid(unsafe_code)]` |
| **2.1b–2.4** Phase 2 remaining (`chord` host, `durable`, `env`, `evals`) | PENDING | Unblocked now that telemetry bridge and workspace are landed |
| **3.1–3.2** Phase 3 remaining (`p3`, `p2` + Leptos WASM web UI) | PENDING | Follow Phase 2 |
| **3.3** `pi-rs-p2-mac` | **PASS** | 16/16 Linux tests + `aarch64-apple-darwin` check + real upstream `p2` smoke PASS (live macOS UI deferred per ADR-005) |
| **3.4** `pi-rs-voice-server` | **PASS** | 17/17 offline tests + real Supertonic/Kokoro/Silero/Parakeet verified |

## Key Links

- Plan & unit claims: [MAP.md](MAP.md) | Decisions: [DECISIONS.md](DECISIONS.md) | Glossary: [GLOSSARY.md](GLOSSARY.md) | Leaf map: [PORT_MAP.md](PORT_MAP.md) | Workspace: [crates/Cargo.toml](crates/Cargo.toml)

## Known Hazards & Notes

1. **Workspace layout**: Cargo workspace root lives at `rust/crates/Cargo.toml` (not `rust/Cargo.toml`) so `rust/pi-rs-*` local `rust-anything` N-API proof crates stay isolated outside Cargo's workspace discovery and are ignored by `/pi-rs-*/` in `rust/.gitignore`.
2. **Remaining milestone caveats**:
   - `pi-rs-coding-agent`: `wit/extension.wit` covers core extension lifecycle/hooks/tools/commands; expand full 41-event WIT surface alongside 1.8b-2/2.1b as needed.
   - `pi-rs-chord`: live reads rerun plans per commit rather than upstream incremental invalidation; transport ports await 2.1b host binding.
   - `pi-rs-p2-mac`: macOS signing and live AppKit/WKWebView UI remain untested on Linux host (ADR-005).
   - `pi-rs-voice-server`: Pocket TTS is a discovery/streaming trait hook pending a native ONNX/Candle adapter; Kokoro requires `espeak-ng`.
3. **Git & privacy safety**: `packages/` and `apps/` untouched; repo-wide privacy audit clean; no `.node` binaries, `.log` files, or `target/` directories tracked.
