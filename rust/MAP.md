# `pi-rs` Whole-App Port Map

**Goal**: Port the entire `earendil-works/pi` monorepo (v1.6.0, baseline commit `780bf31fc`) to 100% Rust with maximalist drop-in compatibility at every external edge, using `rust-anything` for differential verification and keeping upstream code as reference until lifted.

**Core references**:
- Decisions: [DECISIONS.md](DECISIONS.md)
- Terminology & Rust symbol map: [GLOSSARY.md](GLOSSARY.md)
- Current session handoff: [HANDOFF.md](HANDOFF.md)
- Generated port map (created once crates exist): `rust/PORT_MAP.md` (`rust-anything map --write`)

## Current wave claims

| Unit | Owner | Scope |
| --- | --- | --- |
| 2.2b | Astra Low #1 | Durable harness |
| 2.2b observation | Astra Low #2 | Conversation observation and task-graph watches against the durable harness snapshot |
| 2.2b telemetry | Sol Medium #2 | Content-free submission and phase traces, commit events and task metrics |
| 2.3b differential verification | Astra Low #5 | Daemon-backed local and remote file listing against upstream |
| 3.2 worker routing | Sol Medium #1 | Automatic Chord session routing, idle shutdown, retry and catalog integration |
| 3.2 TUI launcher | Astra Low #3 | Native p2 TUI entry, fixed-origin connection and mounted terminal lifecycle |
| 3.2 stale browser page | Coordinator | Authenticated WebSocket version check before attaching a Chord link |
| 3.2 browser identity | Coordinator | Serve the authenticated caller's Access variant and session grants from the server core |
| 3.2 development controls | Astra Low #4 | Private development command socket, reload state and loaded file changes |
| 3.2 runtime logs | Astra Low #6 | Bounded process logs, redaction, snapshots and notice publication used by hosts and plugins |

## ADR Index

| ADR | Title | Status |
| --- | --- | --- |
| ADR-001 | Full-Monorepo Scope and Bottom-Up Phase Order | accepted |
| ADR-002 | 100% Rust Codebase and Guest Runtime Boundaries | accepted |
| ADR-003 | Maximalist Drop-In Compatibility Through Full Parity | accepted |
| ADR-004 | `rust-anything` Strangler Workflow and Repository Layout | accepted |
| ADR-005 | Target Platforms and Rust Baseline | accepted |
| ADR-006 | Cargo Crate and Binary Naming | accepted |
| ADR-007 | Core Rust Dependency Stack and Research Gate | accepted |
| ADR-008 | Tangled Clusters and False Blockers | accepted |
| ADR-009 | Checkpoint Cadence and Session Handoff | accepted |
| ADR-010 | Host QuickJS-WASM Inside Pure-Rust `wasmtime` for `pi-rs-codemode` | accepted |
| ADR-011 | Use `leptos` (`0.8.x`, CSR) for Rust-to-WASM Web Frontends | accepted |

## Pre-Work Prerequisite

- [x] Hydrate Node dependencies (`npm install --ignore-scripts`) — completed 2026-11-21 (336 packages installed; `packages/telemetry` 52/52 tests pass, `packages/schema` 8,744/8,744 tests pass).

## Research Queue (`rust/research/`)

Write each primary-source research note before porting any unit that depends on it (ADR-007).

| Topic | Blocks Package(s) | State | File |
| --- | --- | --- | --- |
| `tui-deps` (`get-east-asian-width`, `marked` + `marked-cjk-friendly`, native clipboard/modifiers) | `pi-rs-tui` | done (2026-11-21) | [rust/research/tui-deps.md](research/tui-deps.md) |
| `codemode-boa` (`boa_engine` async/Promises/limits vs `quickjs-wasi`) | `pi-rs-codemode` | done (2026-11-21) | [rust/research/codemode-boa.md](research/codemode-boa.md) |
| `ai-providers` (10 chat APIs, AWS Bedrock SigV4/event-stream, OAuth, proxies, `partial-json`) | `pi-rs-ai` | done (2026-11-21) | [rust/research/ai-providers.md](research/ai-providers.md) |
| `wasm-extensions` (`wasmtime` + WIT component model for `jiti` & `esbuild` plugins) | `pi-rs-coding-agent`, `pi-rs-chord` | done (2026-11-21) | [rust/research/wasm-extensions.md](research/wasm-extensions.md) |
| `coding-agent-deps` (`proper-lockfile`, `photon-rs`, `highlight.js`/`syntect`, `lovely-mermaid`, `hosted-git-info`, `yaml`) | `pi-rs-coding-agent` | done (2026-11-21) | [rust/research/coding-agent-deps.md](research/coding-agent-deps.md) |
| `chord-storage-sync` (`rusqlite`, Chord v2 SQLite migrations/store/delta/IPC/WS) | `pi-rs-chord`, `pi-rs-durable` | done (2026-11-21) | [rust/research/chord-storage-sync.md](research/chord-storage-sync.md) |
| `web-ui-wasm` (Leptos vs Dioxus for `chord/web`, `p2`, `p3`) | `pi-rs-chord`, `pi-rs-p2`, `pi-rs-p3` | done (2026-11-22) | [rust/research/web-ui-wasm.md](research/web-ui-wasm.md) |
| `p2-extras` (Discord crate, `p2-mac` `wry`/`objc2`, `voice-server` `ort`/ONNX) | `pi-rs-p2`, `pi-rs-p2-mac`, `pi-rs-voice-server` | done (2026-11-22) | [rust/research/p2-extras.md](research/p2-extras.md) |

---

## Phase 1 — Core `pi` CLI & Libraries

### Milestone 1.1: `pi-rs-schema` (`packages/schema` — 4.9k src LOC, 18 units, 0 external deps)
Shape: library. Tangled cluster: `builders.ts + typebox-static.ts` (287 L, ported together per ADR-008). Completed by `t3r` (omp / MiniMax-M3.1-Flash) and independently verified by coordinator 2026-11-21. Uses `regress` (V8 Irregexp port) for exact JS regex semantics; documented known differences in [rust/crates/pi-rs-schema/README.md](crates/pi-rs-schema/README.md).

| Wave | Unit | LOC | Status | State | Evidence |
| ---: | --- | ---: | --- | --- | --- |
| 0 | `packages/schema/src/shared.ts` | 29 | ready | **done** (`t3r`, verified 2026-11-21) | `rust/pi-rs-schema-shared`: `gate` **PASS** (**319/319** match, 28/29 cov) |
| 1 | `packages/schema/src/formats/typebox/idna/format/` | 273 | ready | **done** (`t3r`, verified 2026-11-21) | `rust/pi-rs-schema-idna-format`: `gate` **PASS** (**1,742/1,742** match, 371/371 = 100% cov, 1.2x size) |
| 0–6 | Full `packages/schema/src/` (`types`, `builders`+`typebox-static`, `checks`, `resolver`, `annotations`, `compiler`, `coerce`, `explain`, `formats`+`idna`, `validator`, `index`) | 4,852 | ready/caution | **done** (`t3r`, verified 2026-11-21) | `rust/crates/pi-rs-schema`: `fmt`/`clippy -D warnings` PASS, 0 unsafe/allow, **118/118** tests PASS (**4,213/4,213** official JSON Schema Test Suite draft 2020-12 & draft 7 + **204** TypeBox 1.3.27 `Value.Convert` cases) |

### Milestone 1.2: `pi-rs-telemetry` (`packages/telemetry` — 1.6k src LOC, 6 units, 0 external deps)
Shape: library. Tangled cluster: `index.ts + memory.ts + metrics.ts + noop.ts` (1,022 L, ported together per ADR-008). Completed by `t3q` (jcode / GPT-6.1 Sol) and independently verified by coordinator 2026-11-21. Extended 2026-11-22 with backwards-compatible `TelemetryRecorderSink`, `Context::with_sink`/`with_clock_and_sink`, and `FallibleTelemetryStore` traits (`tests/bridge.rs`, 56/56 total tests PASS) for `pi-rs-chord`'s SQLite backend.

| Wave | Unit | LOC | Status | State | Evidence |
| ---: | --- | ---: | --- | --- | --- |
| 0 | `packages/telemetry/src/{index,memory,metrics,noop}.ts` | 1,022 | ready/caution (cycle) | **done** (`t3q`, verified 2026-11-21) | `rust/crates/pi-rs-telemetry`: `fmt`/`clippy -D warnings` PASS, **53/53** runtime tests + **2/2** compile-fail doctests + child-process propagation PASS |
| 1 | `packages/telemetry/src/render.ts` | 66 | ready | **done** (`t3q`, verified 2026-11-21) | `rust/pi-rs-telemetry-render`: `gate` **PASS** (152L = 2.7x, 0 unsafe, **210/210** diff, **66/66** cov, bench 0.27x N-API) |
| 1 | `packages/telemetry/src/testing/` | 385 | caution | **done** (`t3q`, verified 2026-11-21) | Included in `rust/crates/pi-rs-telemetry` (`conformance` 15/15 PASS) |

### Milestone 1.3: `pi-rs-mcp` (`packages/mcp` — 4.9k src LOC, 13 units, 139 oracle tests)
Shape: library/client (+ OAuth loopback callback server). Zero internal deps. Completed by `opencode` (`t3s`) and independently verified by coordinator 2026-11-21.

| Wave | Unit | LOC | Status | State | Evidence |
| ---: | --- | ---: | --- | --- | --- |
| 0 | Leaf proofs: `src/naming.ts`, `src/result-schema.ts`, `src/input-schema.ts` | 81 | ready | **done** (`opencode`, verified 2026-11-21) | 3/3 `rust-anything gate` **PASS**: `naming` **464/464** (77% cov), `result-schema` **202/202** (100% cov), `input-schema` **204/204** (100% cov) |
| 0–3 | Full `packages/mcp/src/` (`auth-provider`, `config`, `protocol`, `output-limit`, `transports` stdio/pipe/streamable-http/in-memory, `oauth` discovery/callback/loopback/flow/sign-in, `client`, `model-content`, `testing`, `rust/crates/pi-rs-mcp`) | 4,900 | ready/caution | **done** (`opencode`, verified 2026-11-21) | `rust/crates/pi-rs-mcp`: `fmt`/`clippy -D warnings` PASS, 0 `unsafe` in library (1 in test fixture with `// SAFETY:`), 0 `#[allow]`/`unwrap`/`expect`/`panic!`, **142/142** tests PASS (4 unit + 137 integration + 1 doctest; 1 JS-only `this`-receiver test has no Rust equivalent) |

### Milestone 1.4: `pi-rs-codemode` (`packages/codemode` — 3.1k src LOC, 9 units, 231 oracle tests)
Shape: library. Governed by ADR-010 (pure-Rust `wasmtime` 50.0.2 hosting vendored MIT `quickjs.wasm` 3.6.2). Completed by `t3q` (jcode / GPT-6.1 Sol) and independently verified by coordinator 2026-11-21. Details and native adaptations recorded in [rust/crates/pi-rs-codemode/MILESTONE.md](crates/pi-rs-codemode/MILESTONE.md) and [rust/crates/pi-rs-codemode/COVERAGE.md](crates/pi-rs-codemode/COVERAGE.md).

| Wave | Unit | LOC | Status | State | Evidence |
| ---: | --- | ---: | --- | --- | --- |
| 0 | `src/identifier.ts` | 12 | ready | **done** (`t3q`, verified 2026-11-21) | `rust/pi-rs-codemode-identifier`: `gate` **PASS** (**246/246**, 100% cov, 0 unsafe) |
| 0 | `src/source.ts` | 115 | ready | **done** (`t3q`, verified 2026-11-21) | `rust/pi-rs-codemode-source`: `gate` **PASS** (**244/244**, 100% cov, 1.0x, 0 unsafe) |
| 2 | `src/declarations.ts` | 326 | ready | **done** (`t3q`, verified 2026-11-21) | `rust/pi-rs-codemode-declarations`: `gate` **PASS** (**684/684**, 80% cov, 2.0x, 0 unsafe) |
| 1–4 | `src/runtime/` + `rust/crates/pi-rs-codemode` (`wasmtime` + `quickjs.wasm`) | 2,259 | caution | **done** (`t3q`, verified 2026-11-21) | `rust/crates/pi-rs-codemode`: `fmt`/`clippy -D warnings` PASS, `#![forbid(unsafe_code)]`, 0 unsafe/allow/unwrap, **273/273** tests PASS (all 1,174 leaf cases, >1,000-frame catchable `RangeError`, 8 MiB heap caps, deterministic `start()`, worker/inline/remote, real child-process lifetime & UTC isolation) |

### Milestone 1.5: `pi-rs-tui` (`packages/tui` — 21.5k src LOC, 30 units, 10 waves)
Shape: library. Research complete ([rust/research/tui-deps.md](research/tui-deps.md)). Zero cycles, zero blocked units. Completed by `t3r` (omp / `gpt-6.2-sol`) and independently verified by coordinator 2026-11-21. Details and interface contracts recorded in [rust/crates/pi-rs-tui/README.md](crates/pi-rs-tui/README.md) and [rust/crates/pi-rs-tui/PORTING.md](crates/pi-rs-tui/PORTING.md).

| Wave | Units | Total LOC | State | Evidence |
| ---: | --- | ---: | --- | --- |
| 0–2 | Leaf proofs: `fuzzy.ts`, `terminal-colors.ts`, `oklab.ts`, `colors.ts`, `keys.ts` | 2,000+ | **done** (`t3r`, verified 2026-11-21) | 5/5 `rust-anything gate` **PASS**: `fuzzy` 354/354, `terminal-colors` 615/615, `oklab` 1,129/1,129 (100% cov), `colors` 2,960/2,960 (99% cov), `keys` 2,724/2,724 (94% cov) — **7,782/7,782** total |
| 0–9 | Full `packages/tui/src/` (utils, ANSI/Unicode, markdown/CJK, latex, terminal/PTY/X11, layout, main & alt screens, 18 components, `rust/crates/pi-rs-tui`) | 21,500 | **done** (`t3r`, verified 2026-11-21) | `rust/crates/pi-rs-tui`: `fmt`/`clippy -D warnings` PASS, 0 `#[allow]`, `unsafe` isolated to `terminal/platform/` with `// SAFETY:`, **96/96** tests PASS (incl. 629 upstream oracle cases/traces/frames + Linux PTY/X11) |

### Milestone 1.6: `pi-rs-ai` (`packages/ai` — 29.2k src LOC, 33 units)
Shape: library + `pi-ai` CLI. Depends on `pi-rs-schema`, `pi-rs-telemetry`. Research complete ([rust/research/ai-providers.md](research/ai-providers.md)). Completed by `omp` (`GPT-6.2-Sol`) and independently verified by coordinator 2026-11-21. Evidence recorded in [rust/crates/pi-rs-ai/README.md](crates/pi-rs-ai/README.md).

| Wave | Units | Total LOC | State | Evidence |
| ---: | --- | ---: | --- | --- |
| 2 | Leaf proofs: `src/cache-warming.ts`, `src/image-models.ts` | 152 | **done** (`omp`, verified 2026-11-21) | 2/2 `rust-anything gate` **PASS**: `cache-warming` **713/713** (0 unsafe), `image-models` **172/172** (100% cov, 0 unsafe) |
| 0–5 | Full `packages/ai/src/` (10 chat + 1 image + 4 classifier APIs, 9 OAuth flows, ~42 providers, embedded catalog JSON, `pi-ai` CLI binary, `rust/crates/pi-rs-ai`) | 29,200 | **done** (`omp`, verified 2026-11-21) | `rust/crates/pi-rs-ai`: `fmt`/`clippy -D warnings` PASS, 0 `unsafe`, 0 `#[allow]`, **39/39** offline loopback tests PASS in 1.66s |

### Milestone 1.7: `pi-rs-agent` (`packages/agent` — 2.5k src LOC, 6 units)
Shape: library. Depends on `pi-rs-ai`, `pi-rs-schema` (both PASS). Zero cycles, zero external deps. Completed by `omp` (`GPT-6.2-Sol`) and independently verified by coordinator 2026-11-22. Evidence recorded in [rust/crates/pi-rs-agent/README.md](crates/pi-rs-agent/README.md).

| Wave | Units | Total LOC | State | Evidence |
| ---: | --- | ---: | --- | --- |
| 0-4 | Full `packages/agent/src/` (`types`, `stream-fn`, `proxy`, `agent-loop`, `agent`, `index`, `rust/crates/pi-rs-agent`) | 2,500 | **done** (`omp`, verified 2026-11-22) | `rust/crates/pi-rs-agent`: `fmt`/`clippy -D warnings` PASS, 0 `unsafe`/`#[allow]`/`unwrap`/`expect`/`panic!`, **32/32** offline tests PASS in 0.95s (faux registry + loopback Axum SSE + upstream steering/ordering parity; upstream oracle 96/96) |

### Milestone 1.8: `pi-rs-coding-agent` (`packages/coding-agent` — 82.2k src LOC, 29 units, binary `pi`)
Shape: CLI (`pi`). Research complete ([rust/research/wasm-extensions.md](research/wasm-extensions.md), [rust/research/coding-agent-deps.md](research/coding-agent-deps.md)). Split into 1.8a (standalone foundation, **DONE**), 1.8b-1 (core runtime, sessions, compaction, extensions, built-ins), and 1.8b-2 (CLI, modes, interactive TUI, `pi` binary & whole-app replay).

| Wave | Subsystems | Total LOC | State | Evidence |
| ---: | --- | ---: | --- | --- |
| 1.8a | Leaf proofs (`changelog`, `frontmatter`, `ansi`) + standalone foundation (`config`, `migrations`, `lockfile`, standalone `utils/`, `core/{keybindings,skills,prompt_templates,project_trust}`, pure `core/tools/` executors) | ~15k | **done** (`opencode`, verified 2026-11-22) | 3/3 leaf gates **PASS** (`changelog` 898/898, `frontmatter` 230/230 100% cov 1.64x, `ansi` 711/711 100% cov); `rust/crates/pi-rs-coding-agent`: `fmt`/`clippy -D warnings` PASS, 0 `unsafe`/`#[allow]`, **316/316** tests PASS in 0.37s |
| 1.8b-1 | Core data & session runtime: `session-manager` (JSONL v3), `settings-manager`, `auth-storage`, `model-registry`, `system-prompt`, `compaction/`, `AgentTool` wrappers, `extensions/` (41-event trait + checked-in `wit/extension.wit` `wasmtime::component::bindgen!` host), built-in extensions (`codemode`, `mcp`, `tool-search`, `llama`), `agent-session`/`sdk`, `export-html/`, `package-manager` | ~40k | **done** (`opencode` + coordinator integration, verified 2026-11-22) | `rust/crates/pi-rs-coding-agent`: `fmt`/`clippy -D warnings` PASS in unified `rust/crates/Cargo.toml` workspace, `pi-rs-tui` wired directly, real on-disk `tests/fixtures/echo_extension.wasm` WIT component test PASS, 0 `unsafe`/`#[allow]`, **759/759** offline tests PASS in 18.88s; details in [MILESTONE.md](crates/pi-rs-coding-agent/MILESTONE.md) |
| 1.8b-2a | Non-interactive CLI & front door: `cli/` (15 files), `modes/{print,json,rpc}`, `package_manager_cli`, `main.ts`/`rpc-entry.ts`, `[[bin]] name = "pi"` + whole-app `app record/replay` for non-interactive paths | ~5.5k | **done** (`omp`, verified 2026-11-22) | `rust/crates/pi-rs-coding-agent` (`pi` bin): `fmt`/`clippy -D warnings` PASS, `#![forbid(unsafe_code)]`, 0 `#[allow]`, **806/806** tests PASS in 22.4s + 8/8 TS oracle replay + loopback print/15-event JSON parity; details in [MILESTONE.md](crates/pi-rs-coding-agent/MILESTONE.md) |
| 1.8b-2b | Interactive TUI mode: `modes/interactive/` (`theme/`, 45 `components/`, `chat_viewport`, `tui_renderer`, `interactive_mode`, `external_editor`, `model_search`, `session_share`, `bug_report`) wired to `pi-rs-tui` | ~22.8k | **done** (`omp`, verified 2026-11-22) | `rust/crates/pi-rs-coding-agent` (`pi` bin): `fmt`/`clippy -D warnings` PASS, `#![forbid(unsafe_code)]`, 0 `#[allow]`, **865/865** tests PASS (+59 new, 5/5 mutation-checked + real Linux PTY main/alt/resume/config checks) -> **Phase 1 100% COMPLETE**; details in [MILESTONE.md](crates/pi-rs-coding-agent/MILESTONE.md) |
| 1.8c | Phase 1 Parity Polish: standalone `pi mcp` CLI, live MCP tool/OAuth wiring, expanded WIT guest UI callbacks, live extension provider/virtual-model reload | polish | **done** (`codex / GPT-6.2-Astra`, verified 2026-11-22) | `rust/crates/pi-rs-coding-agent` (`pi` bin): `fmt`/`clippy -D warnings` PASS, `#![forbid(unsafe_code)]`, 0 `#[allow]`, **879/879** tests PASS (+14 new, all 14 mutation-checked: standalone `pi mcp` CLI, live stdio/HTTP MCP + stored OAuth token execution, expanded WIT guest UI/registration callbacks, live extension provider/virtual-model reload); details in [MILESTONE.md](crates/pi-rs-coding-agent/MILESTONE.md) |

---

## Phase 2 — Platform Runtime

| Milestone | Crate | Upstream Package | Src LOC | Units / Pieces | State | Dependencies |
| --- | --- | --- | ---: | --- | --- | --- |
| 2.1a | `pi-rs-chord` (Foundation: `json`, `output`, `context`, `watch`, `data`, `contracts`, `delta`, `state`, `documents`, `sqlite`, `store`, `sync`, `link`, `telemetry-sqlite`, `telemetry-ingest`) | `packages/chord` | ~29k | 16 subsystems | **done** (`jcode` + coordinator telemetry bridge, verified 2026-11-22) | 2/2 leaf gates **PASS** (`json` 151/151, `output` 4,844/4,844); `rust/crates/pi-rs-chord`: `fmt`/`clippy -D warnings` PASS, `#![forbid(unsafe_code)]`, 0 `unwrap`/`expect`/`panic!`, **107/107** offline tests PASS incl. all 19 untouched-TS binary & segmented-text goldens + `tests/telemetry_bridge.rs`; fidelity limits recorded in [MILESTONE.md](crates/pi-rs-chord/MILESTONE.md) |
| 2.1b-1 | `pi-rs-chord` (Portable Runtime: `host`, `jobs`, `registries`, `remote`, `surfaces`, `tui`, `web`) | `packages/chord` | ~25k | 7 subsystems | **done** (`jcode`, verified 2026-11-22) | `rust/crates/pi-rs-chord`: `fmt`/`clippy -D warnings` PASS, `#![forbid(unsafe_code)]`, 0 `#[allow]`/`unwrap`/`expect`/`panic!` in `src/`, **143/143** tests PASS (+36 new, all 36 mutation-checked); details & fidelity limits in [MILESTONE.md](crates/pi-rs-chord/MILESTONE.md) |
| 2.1b-2 | `pi-rs-chord` (Node/Platform Host: `node/{http,workers,plugins,surfaces,tui,app}`, WASM/WIT plugins) | `packages/chord` | ~14.6k | 6 subsystems | **done (bounded)** (`jcode`, verified 2026-11-22) | `rust/crates/pi-rs-chord`: `fmt`/`clippy -D warnings` PASS, `#![forbid(unsafe_code)]`, 0 `#[allow]`, **163/163** tests PASS (+20 new, all 20 mutation-checked: live Axum HTTP/WS RPC, `wit/plugin.wit` bindgen, real child workers, surfaces/TUI/app auth/files/archive); remaining HostData address-ownership/worker-routing/registry-adapter gaps logged in [MILESTONE.md](crates/pi-rs-chord/MILESTONE.md) |
| 2.1b-3 | `pi-rs-chord` (Platform Orchestration Gap Closure: `HostData` address ownership/export/import/delete, worker routing, registry-bound `Http`/`Files`/`Users`, guest WASM fixture) | `packages/chord` | gap closure | 4 areas | **done** (`jcode / GPT-6.2 Sol`, verified 2026-11-22) | `rust/crates/pi-rs-chord`: `fmt`/`clippy -D warnings` PASS, `#![forbid(unsafe_code)]`, 0 `#[allow]`, **178/178** tests PASS (+15 new, all 15 mutation-checked, real checked-in WASM guest components, `HostData` owner transfer, worker routing, registry-bound `Http`/`Files`/`Users`, TUI launcher); merged into `main` |
| 2.2a | `pi-rs-durable` (Foundation, Env, Images, Tools, Tool-Search, Codemode, Storage backends & Session engine) | `packages/durable` | ~23.4k | 18 units | **done** (`omp / GPT-6.2-Sol`, verified 2026-11-22) | `rust/crates/pi-rs-durable`: `fmt`/`clippy -D warnings` PASS, `#![forbid(unsafe_code)]`, 0 `#[allow]`, **77/77** contract tests PASS in 19.80s (memory/SQLite/Chord-store/JSONL storage conformance + local `ExecutionEnv` conformance + 16 mutation checks); details in [MILESTONE.md](crates/pi-rs-durable/MILESTONE.md) |
| 2.2b | `pi-rs-durable` harness | `packages/durable` | ~12.5k | 25 files | Partial | 81 native tests passed on the frozen core. Four new mutation-verified cases cover storage, tool rounds, transcript windows and automatic compaction. Observation, telemetry and lifecycle parity are open. |
| 2.3a | `pi-rs-env` (Daemon + Transport: integrated `packages/env/daemon/` as `pi-env` bin + `connection`, `ssh`, `process`, `terminal`, `watch`, `errors`) | `packages/env` (+ Rust `daemon/`) | 1.5k TS + 5.8k Rust | 6 client units + daemon | **done** (`opencode`, verified 2026-11-22) | `rust/crates/pi-rs-env` (`pi-env` bin): `fmt`/`clippy -D warnings` PASS, 0 `#[allow]`, `unsafe` isolated to `daemon/sys/`, **41/41** tests PASS in 1.17s (33 unit + 8 live `pi-env` binary integration); details in [MILESTONE.md](crates/pi-rs-env/MILESTONE.md) |
| 2.3b | `pi-rs-env` durable bridge | `packages/env` | 1.1k TS | 2 units | Native gates passed, formal listing replay pending | 46 tests. Both adapters agree on live listing, shell output and old-daemon errors for the exercised cases. See [milestone](crates/pi-rs-env/MILESTONE.md). |
| 2.4 | `pi-rs-evals` | `packages/evals` | 1.4k | 7 units | **done** (`opencode`, verified 2026-11-22) | `rust/crates/pi-rs-evals` (`pi-evals` bin): `fmt`/`clippy -D warnings` PASS, `#![forbid(unsafe_code)]`, 0 `#[allow]`, **44/44** tests PASS in 0.02s (5/5 mutation-checked, upstream golden report/SHA parity, TTY-deterministic styling); details in [MILESTONE.md](crates/pi-rs-evals/MILESTONE.md) |

---

## Phase 3 — Applications

| Milestone | Crate | Upstream App | Src LOC | Units / Pieces | State | Notes |
| --- | --- | --- | ---: | --- | --- | --- |
| 3.1 | `pi-rs-p3` | `apps/p3` (`src/` + `plugins/`; `old/` skipped) | 2.5k | 21 src + 4 plugins | **done** (`opencode / DeepSeek` + `codex / GPT-6.2-Astra`, verified 2026-11-22) | `rust/crates/pi-rs-p3` (`p3` bin): `fmt`/`clippy -D warnings` PASS, `#![forbid(unsafe_code)]`, 0 `#[allow]`, **79/79** tests PASS in 1.97s (69 lib + 6 live `p3` binary CLI + 4 live child-worker runner + 5/5 mutation checks); details & `2.1b-3` seam notes in [MILESTONE.md](crates/pi-rs-p3/MILESTONE.md) |
| 3.2-web | `pi-rs-p2-web` (Leptos CSR frontend and browser host) | `apps/p2/src/web`, `apps/p2/src/hosts/web` | ~13.0k | Browser | **partial** | Chromium exchanged chat with the upstream server. A packaged native server then served the embedded WASM, CSS and fonts under production CSP to owner and guest fixtures. Plugin UI loading, native authenticated chat and persistence remain open. [Browser milestone](crates/pi-rs-p2-web/MILESTONE.md), [packaging and gates](crates/pi-rs-p2/MILESTONE.md) |
| 3.2-tui | `pi-rs-p2-tui` mounted client | `apps/p2/src/tui` | ~6.9k | 34 TUI files | **partial** | 20 native tests. Five new mutation-checked cases cover retained transcript rows, panel paging, tree layout, registered shortcuts and atomic theme replacement. Dynamic guest dispatch and whole-screen parity remain open. [Milestone](crates/pi-rs-p2-tui/MILESTONE.md) |
| 3.2 | `pi-rs-p2` (server, runtime, hosts and 49 plugins) | `apps/p2` | ~89.6k | server + runtime + plugins | **partial** | The native core passed six HTTP replay cases. Shared declarations match 106 upstream contracts across 130 versions. Production worker routing, plugin implementations and authenticated native chat remain open. |
| 3.3 | `pi-rs-p2-mac` | `apps/p2-mac` | 3.5k Swift | macOS shell | **done** (`omp`, verified 2026-11-22) | `rust/crates/pi-rs-p2-mac` (`pi-mac` binary): `fmt`/`clippy -D warnings` PASS, **16/16** Linux x86_64 tests PASS (network/portable/supervisor) + `aarch64-apple-darwin` type-check PASS + real upstream `p2` smoke PASS; live macOS AppKit/WKWebView & signing deferred per ADR-005 |
| 3.4 | `pi-rs-voice-server` | `apps/p2/voice-server` | Python | 4 units | **done** (`omp`, verified 2026-11-22) | `rust/crates/pi-rs-voice-server` (`pi-voice-server` binary): `fmt`/`clippy -D warnings` PASS, **17/17** offline HTTP/WS/segmenter/text tests PASS in 0.74s + real Supertonic 44.1 kHz, Kokoro 24 kHz, and Silero/Parakeet (`sherpa-onnx 1.13.8` parity) verified; Pocket synthesis hook ready for native ONNX/Candle adapter |

---

## Open Questions

1. **Resolved (2026-11-22)**: Skip `apps/p3/old/` as dead archive code; port only live `apps/p3/src/` and `apps/p3/plugins/`.
2. **Resolved (2026-11-22, ADR-011)**: Adopted `leptos` (`0.8.x`, CSR) per [web-ui-wasm.md](research/web-ui-wasm.md) for drop-in CSS/WebKit compatibility, 1:1 Chord reactive signal parity, and shared server/WASM wire types.
3. **Resolved (2026-11-22)**: `packages/ai/src/providers/data/` is already hydrated on disk (matching `pi-rs-ai`'s embedded catalog); use those local provider JSON files offline when recording upstream `pi` fixtures.
