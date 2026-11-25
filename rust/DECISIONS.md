# Architecture Decision Records (`pi-rs`)

Numbered in order; never renumbered. A changed decision gets a new ADR that supersedes the old one.

## ADR-001: Full-Monorepo Scope and Bottom-Up Phase Order
Status: accepted (2026-11-21)
Context: `pi-rs` is forked from `earendil-works/pi` v1.6.0 (`780bf31fc`, 12 packages + 3 apps, ~359k TS/JS LOC across 1,743 source files, plus 5.8k Rust LOC in `packages/env/daemon`, 3.5k Swift LOC in `apps/p2-mac`, and Python `apps/p2/voice-server`). Porting leaf libraries first avoids bridging cycles across packages and delivers a usable standalone `pi` CLI early.
Decision: Port the entire monorepo to 100% Rust in three bottom-up phases:
1. **Phase 1 — Core `pi` CLI**: `schema`, `telemetry` -> `tui`, `mcp`, `codemode`, `ai` -> `agent` -> `coding-agent` (`pi` CLI in interactive, print, JSON, and RPC modes).
2. **Phase 2 — Platform Runtime**: `chord` -> `durable` -> `env` (integrating existing Rust `packages/env/daemon`) + `evals`.
3. **Phase 3 — Apps**: `p3`, `p2` (server + Rust/WASM web UI + TUI), `p2-mac`, `voice-server`.
Consequences: Each phase yields verified crates and runnable binaries before starting the next layer. Phase 3 UI and ML inference tracks do not block Phase 1 delivery of `pi`.

## ADR-002: 100% Rust Codebase and Guest Runtime Boundaries
Status: accepted (2026-11-21)
Context: Upstream relies on non-Rust runtimes at four edges: dynamic `.ts` extensions (`jiti`) and Chord hot-reload plugins (`esbuild`), LLM-authored JavaScript in `codemode` (`quickjs-wasi`), React/TS web UI (`chord/web`, `chord/react`, `apps/p2`), and non-TS extras (`apps/p2-mac` in Swift, `apps/p2/voice-server` in Python).
Decision: All authored source code and build tooling in `pi-rs` is 100% Rust:
- **Built-in extensions and Chord plugins**: compiled directly into Rust via trait interfaces.
- **Dynamic / hot-reloadable extensions and Chord plugins**: WASM Component Model (`wasm32-wasip2`) hosted by `wasmtime` with explicit capability sandboxing.
- **Codemode (`pi-rs-codemode`)**: pure-Rust JavaScript interpreter (`boa_engine`) executing LLM-authored async scripts with zero ambient FS/net/timer capabilities, exposing upstream's `tools`, `models`, `store`/`load`, `text`, `image`, and `console` globals.
- **Web UI (`chord/web`, `p2`/`p3` frontend)**: Rust compiled to WASM (framework—Leptos vs Dioxus—finalized via `rust/research/web-ui.md` before Phase 3), sharing `pi-rs-chord` protocol and schema crates directly with the server.
- **Non-TS extras**: `p2-mac` ported to Rust (`wry`/`objc2`/`tray-icon`); `voice-server` ported to Rust (`axum` + `ort` / `candle` over the existing ONNX/model weights).
Consequences: Zero Node.js, esbuild, Python, or Swift runtime dependencies in lifted binaries. Existing upstream `.ts` user extensions will not execute directly without porting to WASM/Rust (an optional external RPC extension bridge remains possible later). Capability isolation is stronger than upstream's Node `--permission` worker hack.

## ADR-003: Maximalist Drop-In Compatibility Through Full Parity
Status: accepted (2026-11-21)
Context: A ~360k-LOC port risks silent behavioral drift unless held to an automated oracle. Existing users and tools also depend on `~/.pi/agent/` state, JSONL v3 sessions, and RPC/JSON protocols.
Decision: Maintain maximalist drop-in compatibility at every external edge until full parity is reached, and strongly favor preserving drop-in compatibility thereafter:
- Exact CLI flags, subcommands, stdout/stderr structure, and exit codes.
- Exact `~/.pi/agent/` files (`settings.json`, `auth.json`, `models.json`, `keybindings.json`, `themes/*.json`, trust store).
- Exact JSONL v3 tree session files (`id`/`parentId`, compaction, branch summaries, context edits, system message section diffs).
- Exact `--mode json` event stream and `--mode rpc` strict LF-framed JSONL protocol.
- Exact HTTP routes, status codes, and SQLite schemas for `chord`, `durable`, `p3`, and `p2` (the single intentional wire swap is replacing `p2`'s Node JS TUI bundle transport with WASM/Rust per ADR-002).
- Equivalence bar: exact match on deterministic outputs; normalize only genuine run-to-run noise (timestamps, UUIDs, unordered concurrency where verified) in recorded cases, never in ports.
Consequences: `rust-anything record`/`diff`/`app replay` can treat the untouched upstream TS codebase as ground truth. Rough edges are only reconsidered after full parity is proven.

## ADR-004: `rust-anything` Strangler Workflow and Repository Layout
Status: accepted (2026-11-21)
Context: `rust-anything` (`whole-app.md`) provides incremental verification: `analyze` -> `bridge` -> `record` -> port `src/port.rs` -> `build` -> `diff` -> `bench` -> `shadow` -> `gate`, plus CLI/server `app record` -> `app front-door` -> `app wire` -> `app lift`.
Decision: Keep all upstream source trees (`packages/`, `apps/`) untouched as reference and behavioral oracle until each target is lifted. Place all port state, plans, research, recorded cases, unit crates, and front doors under `rust/`:
- `rust/MAP.md` — cross-session plan, package/unit tracker, and claims.
- `rust/DECISIONS.md` — this ADR log.
- `rust/GLOSSARY.md` — upstream-to-Rust term and symbol mapping.
- `rust/HANDOFF.md` — concise current-state handoff (rewritten, <150 lines).
- `rust/PORT_MAP.md` — generated by `rust-anything map --write`.
- `rust/research/<topic>.md` — primary-source crate/dependency research written before porting dependent units.
- `rust/app-cases/` — recorded trycmd/HTTP whole-app behavior cases.
- Unit crates and front-door crates under `rust/`.
Consequences: Every ported unit is proven against recorded upstream behavior before wiring, with kill switches (`RUST_ANYTHING_UNIT_<NAME>=original`) until `app lift`. Installing upstream npm deps (`npm install --ignore-scripts`) is required locally to run `record` and `app record` against the TS original.

## ADR-005: Target Platforms and Rust Baseline
Status: accepted (2026-11-21)
Context: Local development environment is Linux x64 (`rustc 1.98.0`, `cargo 1.98.0`, `wasm32-unknown-unknown` installed). Linux arm64 test environment can be provided on demand; macOS and dedicated Windows machines are not currently available for live testing. Upstream's `packages/env/daemon/Cargo.toml` already uses Rust edition 2024.
Decision:
- **Rust baseline**: Stable Rust, `edition = "2024"`.
- **Primary dev/test target**: `x86_64-unknown-linux-gnu`.
- **Secondary testable target**: `aarch64-unknown-linux-gnu` (tested on demand).
- **Portable build targets**: `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc`, and Linux/Android Termux where upstream supports it; keep code cross-platform and build-checked via `cfg` isolation and CI, deferring live runtime testing on macOS and Windows.
- **Existing Rust code**: Reuse and integrate `packages/env/daemon` rather than rewriting it.
Consequences: Platform-specific code (TTY raw mode, signals, clipboard, process groups/PTYs, paths) must sit behind clean `#[cfg(unix)]` / `#[cfg(windows)]` / `#[cfg(target_os = "macos")]` abstractions from day one.

## ADR-006: Cargo Crate and Binary Naming
Status: accepted (2026-11-21)
Context: Cargo decouples package names from installed binary names. Short generic crate names (`pi-ai`, `pi-tui`, `pi-schema`) risk crates.io collisions and collide with upstream's existing `packages/env/daemon` (`pi-env`).
Decision:
- **Cargo package names**: use the `pi-rs-*` namespace (`pi-rs-schema`, `pi-rs-telemetry`, `pi-rs-tui`, `pi-rs-mcp`, `pi-rs-codemode`, `pi-rs-ai`, `pi-rs-agent`, `pi-rs-coding-agent`, `pi-rs-chord`, `pi-rs-durable`, `pi-rs-env`, `pi-rs-evals`, `pi-rs-p3`, `pi-rs-p2`, `pi-rs-p2-mac`, `pi-rs-voice-server`), marked `publish = false` initially.
- **Binary targets**: preserve exact upstream binary names (`pi`, `p3`, `p2`, `pi-env`) via `[[bin]] name = "..."`.
Consequences: CLI usage text, scripts, and `app replay` stay drop-in compatible while keeping the Cargo namespace collision-free long term.

## ADR-007: Core Rust Dependency Stack and Research Gate
Status: accepted (2026-11-21)
Context: `rust-anything analyze --whole-app` identified 1 `fit=none` dependency (`jiti`), 35 `fit=unknown` dependencies, and 2 `fit=partial` dependencies (`yaml`, `numpy`) across the monorepo. Per `whole-app.md`, unknown/high-risk dependency replacements must be verified against primary sources in `rust/research/<topic>.md` before porting units that rely on them.
Decision: Adopt a coherent baseline Rust stack, subject to primary-source verification in `rust/research/` before each dependent package begins:
- **Core runtime & I/O**: `tokio` (async runtime), `clap` (CLI front doors), `serde` + `serde_json` + `indexmap` (order-preserving JSON/maps), `thiserror` / `anyhow` (structured errors; library crates use typed enums).
- **Networking**: `reqwest` (HTTP/SSE + proxy support, replacing `undici` and `http/https-proxy-agent`), `tokio-tungstenite` (WebSockets, replacing `ws`), `axum` (HTTP/WS servers in OAuth callbacks, `chord/http`, `p2`, `p3`, `voice-server`).
- **Provider wire protocols (`pi-rs-ai`)**: Direct HTTP/SSE/WebSocket implementations for all 10 chat APIs, 1 image API, and 4 classifier APIs, plus AWS SigV4 / Bedrock event-stream decoding (`aws-sigv4` / `aws-config` or direct crate verified in research), replacing `@anthropic-ai/sdk`, `openai`, `@google/genai`, `@aws-sdk/*`, and `partial-json` (ported directly).
- **Schema & regex**: `pi-rs-schema` ported directly from `packages/schema` (zero external deps, replacing both `pi-schema` and runtime `typebox` usage); `regex` for standard patterns and `fancy-regex` where JS lookaround/backreferences occur.
- **TUI & text**: `unicode-width` / East Asian width table verified against `get-east-asian-width`, `pulldown-cmark` + CJK emphasis handling verified against `marked` + `marked-cjk-friendly`, `crossterm` / raw termios + native platform clipboard/modifier bindings, `syntect` (replacing `highlight.js`/`lowlight`), pure-Rust Mermaid ASCII/SVG rendering (replacing `lovely-mermaid`/`grok-mermaid`).
- **Files, locks, media & config**: `ignore` + `globset` + `similar` (diff), `saphyr` / `serde_yaml_ng` (frontmatter/YAML parsing; confirm comment-preserving rewrite is unused), `semver`, `image` / `photon-rs` (replacing `@silvia-odwyer/photon*`, which is itself Rust!), `fs2` / atomic lockfile semantics verified against `proper-lockfile`.
- **Sandboxing & plugins**: `boa_engine` (`quickjs-wasi` replacement), `wasmtime` (`jiti` + `esbuild` replacement).
- **Storage & Phase 2/3**: `rusqlite` (bundled SQLite for `chord`/`durable`/`telemetry-sqlite`), `serenity` or `twilight` (Discord in `p2`), `ort` + `ndarray` (`voice-server`), Rust-WASM frontend (`leptos` or `dioxus`).
Consequences: No dependency is chosen blindly; before porting any package that depends on an unverified replacement, a `rust/research/<topic>.md` note documents exact API mappings and semantic traps from primary sources.

## ADR-008: Tangled Clusters and False Blockers
Status: accepted (2026-11-21)
Context: `rust-anything analyze --whole-app` flagged 6 `blocked` units and 16 import cycles (`D144`–`D159`). Direct code inspection showed:
- All 4 `blocked` units in `packages/ai` (`src/api/`, `src/auth/`, `src/auth/oauth/`, `src/env-api-keys.ts`) are false blockers caused solely by JS lazy `import()` shims (`*.lazy.ts`, dynamic `node:fs`/`node:crypto` imports for browser bundling), with zero `eval`/`new Function`.
- `packages/coding-agent/src/core/extensions/` is blocked on `jiti` (resolved by ADR-002).
- `apps/p2/src/hosts/` is blocked on dynamic `import()` in `tui-bootstrap.mjs` (resolved by ADR-002).
- Import cycles exist inside `schema` (287 L), `telemetry` (1,022 L), `ai` (25.7k L + 3.6k L catalog), `coding-agent` (4.1k L utils + 69k L core), `chord` (1.1k L + 22.3k L), `durable` (18.2k L), and `p2` (several sub-clusters).
Decision:
1. Treat `packages/ai`'s 4 lazy-import units as normal unblocked Rust modules (no dynamic loading needed in Rust).
2. Port each cohesive import cycle as a single Rust module/crate rather than bridging across cycle edges (e.g., `schema`'s `builders.ts + typebox-static.ts` together; `telemetry`'s core 4 files together; `ai` and `coding-agent` structured cleanly into internal Rust modules once their leaf units are ported).
3. Verify whether `apps/p3/old/` is dead archive code before Phase 3 (confirm with user before porting or omitting it).
Consequences: Eliminates unnecessary cross-language glue inside cycles while keeping leaf units small and independently diff-verified.

## ADR-009: Checkpoint Cadence and Session Handoff
Status: accepted (2026-11-21)
Context: A full-monorepo port spans many sessions and potentially multiple parallel workers per wave.
Decision:
- Stop for user review at Checkpoint 1 (after initial `analyze` + `DECISIONS.md` + `MAP.md` + `GLOSSARY.md`), and thereafter at each **package / wave milestone**.
- Before starting any unit, claim it in `rust/MAP.md`; when done, record its `gate` and `diff`/`replay` evidence in `rust/MAP.md`.
- At every checkpoint, run `rust-anything map --write` (once crates exist) and rewrite `rust/HANDOFF.md` (<150 lines) with current commit, completed milestones, fallback rate, next unit, and hazards.
Consequences: Any fresh session can resume cleanly from `rust/MAP.md` and `rust/HANDOFF.md` without re-discovering repo state.

## ADR-010: Host QuickJS-WASM Inside Pure-Rust `wasmtime` for `pi-rs-codemode`
Status: accepted (2026-11-21) — supersedes the `boa_engine` selection for `pi-rs-codemode` in ADR-002 and ADR-007.
Context: Prototyping `pi-rs-codemode` on `boa_engine 0.22` (`rust/crates/pi-rs-codemode/LIMITATIONS.md`) proved two hard incompatibilities against upstream `packages/codemode` (`quickjs-wasi`):
1. `boa_engine`'s `RuntimeLimits::set_recursion_limit` raises an uncatchable `RuntimeLimitError`, whereas upstream requires JS `try/catch` to catch a `RangeError` after >1,000 frames.
2. `boa_engine 0.22` provides no per-realm heap byte cap (unlike QuickJS's `JS_SetMemoryLimit`, which `coding-agent` uses at 256 MiB and remote hosts use at 32 MiB).
Decision: Implement `pi-rs-codemode` with 100% pure-Rust `wasmtime` hosting the MIT-licensed `quickjs-wasi` `quickjs.wasm` guest binary (vendored with its MIT license under `rust/crates/pi-rs-codemode/assets/`). Implement custom WASI `clock_time_get`, `random_get`, `fd_write` discard, and QuickJS host-call / interrupt imports directly in Rust.
Consequences: Zero C build toolchain and 100% authored Rust host code, reusing `wasmtime` already required by ADR-002. Achieves exact upstream parity on catchable `RangeError`, `memoryLimitBytes`, deterministic `start()` clock/PRNG/UTC/no-WeakRef mode, microtask stepping (`JOBS_PER_POLL = 50`), and error formatting. Ships a 637 KiB prebuilt `quickjs.wasm` guest asset.

## ADR-011: Use `leptos` (`0.8.x`, CSR) for Rust-to-WASM Web Frontends
Status: accepted (2026-11-22) — resolves Open Question #2 from [rust/research/web-ui-wasm.md](research/web-ui-wasm.md).
Context: Upstream `apps/p2` (and future `apps/p3` pages) serve pure client-side rendered (CSR) browser apps over Chord binary WebSocket links, reusing upstream CSS design tokens (`elanor.css`, etc.) and raw DOM/WebKit hooks (`p2-mac` `WKWebView`, `ResizeObserver`, Web Audio). Under ADR-002, all web UI code is Rust compiled to `wasm32-unknown-unknown`.
Decision: Adopt `leptos` (`0.8.x`, CSR mode) with `wasm-bindgen` and `web-sys` for all Rust-to-WASM browser UIs, sharing `pi-rs-chord`'s `wasm32`-clean protocol, delta, contract, and surface types directly with the server.
Consequences: Chord's reactive `Readable`/`Document`/`LiveQuery` deltas map 1:1 onto fine-grained Leptos signals with zero VDOM diffing on streaming transcripts, upstream CSS and `p2-mac` DOM/WebKit contracts work verbatim for drop-in compatibility, and server/client wire types stay in a single Rust crate.

## ADR-012: Retire Upstream Reference Trees, Vendor Runtime Assets, and Promote Cargo Workspace to Repo Root
Status: accepted (2026-11-24) — completes the lift phase of ADR-004.
Context: All 18 Rust workspace crates, 8 binaries, 49 built-in `p2` plugins, `p2 tui`, and the Leptos WASM browser UI have passed differential and live E2E gates. Remaining build-time dependencies on upstream trees were limited to static assets in `pi-rs-coding-agent`, JSON Schema test fixtures in `pi-rs-schema`, Elanor CSS/WOFF2 fonts bundled via `esbuild` in `pi-rs-p2-web`, and two upstream-oracle tests in `pi-rs-p2`.
Decision:
1. Vendor all required static assets, conformance fixtures, and permissively licensed Elanor CSS/WOFF2 files (OFL-1.1, ISC, MIT) directly inside their owning Rust crates.
2. Replace `esbuild` with a pure-Rust, zero-extra-dependency CSS and font bundler (`p2-bundle-assets` in `pi-rs-p2-web`), orchestrated by `scripts/package-p2.sh`.
3. Retire `packages/`, `apps/`, and legacy Node/TS/Nix root files, and promote `Cargo.toml`, `Cargo.lock`, and `crates/pi-rs-*` to the repository root while retaining `rust/` for ADRs, port history, research notes, and recorded cases.
4. Add GitHub Actions CI (`.github/workflows/ci.yml`) and tag-driven release packaging (`.github/workflows/release.yml`) targeting the root Cargo workspace with zero Node.js/npm dependencies.
Consequences: Standard `cargo fmt`, `cargo clippy`, `cargo test`, and `./scripts/package-p2.sh` run directly from the repository root with zero Node.js, npm, or `esbuild` dependencies. Upstream `earendil-works/pi` commit `780bf31fc` remains accessible in git history whenever historical comparison is needed.
