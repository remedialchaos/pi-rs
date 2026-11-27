# `pi-rs` Handoff

- **Checkpoint**: **Phase 1** (`v1.6.0` `780bf31fc` full parity + post-ADR-012 remediation, Workstreams `1.1a`–`1.7`) complete and verified across all 18 crates on `main` (uncommitted working tree on top of `69ccbbc42`).
- **Stage**: Pure-Rust standalone monorepo with all 18 crates, all 8 binaries (`pi`, `pi-ai`, `pi-env`, `pi-evals`, `p3`, `p2`, `pi-mac`, `pi-voice-server`), all 49 built-in `p2` plugins, full `p2 tui` + `p2` browser UI parity, native Rust `pi-rs-evals` OCI harness, and Linux external adapters verified.
- **Fallback rate**: 0% fallback; zero runtime or build dependency on Node.js, Swift, or Python.
- **Next action**: Begin **Phase 2 (`v1.6.1`)** parity sync (and optional Group F non-Linux native runtime checks on macOS/Windows).

## Gate Table

| Gate | Status | Evidence |
| --- | --- | --- |
| Alignment, ADRs (`ADR-001`..`ADR-012`), primary-source research (8/8) | **PASS** | [DECISIONS.md](DECISIONS.md); all 8 notes in `rust/research/` |
| Root 18-crate Cargo workspace + zero-Node packaging & CI | **PASS** | `Cargo.toml`, `crates/pi-rs-*`, `scripts/package-p2.sh`, `.github/workflows/{ci,release}.yml` |
| Groups **A–E** baseline parity & Linux external adapters | **PASS** | Merged onto `main` at `69ccbbc42` |
| **Phase 1 — 1.1a (`pi-rs-p2-mac`) & 1.2 (Core Libs)** | **PASS** | Native `p2` launcher + `ServerScan` + `scripts/package-p2-mac.sh`; UTF-16 MCP naming; `pi-rs-ai` `compat::stream` auth, Mistral 60s, Codex HTTPS proxy TLS (`rustls`/`tokio-rustls` edges in `Cargo.lock`), WS debug stats, `pi-ai-catalog`; `pi-rs-agent` `wire_options`; `pi-rs-codemode` APIs; `pi-rs-tui` `#[cfg]` macOS/Windows platform impls; `pi-rs-voice-server` Pocket discovery/docs/`Dockerfile`/`VOICE_POCKET_THREADS` (652 passed, 2 existing ONNX ignored; 20/20 mutations) |
| **Phase 1 — 1.1b (`pi-rs-env`), 1.1c (`pi-rs-durable`), 1.7 (`pi-rs-p3`)** | **PASS** | `RemoteWatcher::run` reconnect/overflow, pipelined `write` (`WRITE_DEPTH=8`), 8-target daemon CI staging; durable scheduler fault/orphan/close-flush fixes + `contextRetentionMs` + atomic create/fork + installable extensions (107/107 contracts); `p3` `/chord` link revocation via `OpenedLinkWithClose`, rolling cookie `secure` provenance, `kill(-pid)`, `install::version() == "0.0.1"` (13/13 mutations) |
| **Phase 1 — 1.1d (`pi-rs-p2` Server & Plugins)** | **PASS** | `radius` `BackupTargets::offer`, `scope-prompt` `MessageContexts`, `codemode` live tools/models/store/`RunningCalls`, `powershell` probe, `agent-plugins` skill, `P2_PROVIDER`/`P2_MODEL`, `/upload`/`/download`/`/owner` routes, `dev-tools`/`sessions`/`envs`/`terminals`/`artifacts` parity, SQLite telemetry + 7 `p2` CLI subcommands (207/207 across 32 binaries; +15 tests; 4/4 mutations) |
| **Phase 1 — 1.1e (`pi-rs-evals`)** | **PASS** | Native Rust docs/host/audit suites, pure-Rust `docker/Dockerfile`, Node/Vitest hardcodes removed, `harness.rs` lifecycle/error/missing-session/`partial_run`/portable UID/GID (65/65; real rootless Podman ran 7 docs cases across 2 images, 14 observations; 13/13 mutations) |
| **Phase 1 — 1.3 (`pi-rs-coding-agent`)** | **PASS** | `AgentHooks`, `ModelRuntime` hooks, async `before_agent_start`, nested tool calls + `parentToolCallId`, 4 built-in extensions (`codemode`, `tool-search`, `llama.cpp`, `mcp`), expanded `wit/extension.wit`, `pi-virtual`, proxy/idle timeout, auth status/resync, `CacheWarmer`/`UsageTotals`, `crashes.json`, bash/ps streaming + cancel, `FileMutationQueue`, migrations/schemas, full 1.3c interactive mode parity (931 passed; +17 tests; 3/3 mutations) |
| **Phase 1 — 1.4 (`pi-rs-chord`)** | **PASS** | Remote `bytes()` + events, local `Host` `keyed`/`queries` + `files/facet.rs`, WASM `plugin.wit` + `host.log`, auth/surfaces/CSP/`formatTime`/`formatNumber` + CLDR `icu_locale` (`Cargo.lock` edge), `admission.rs` bounds, `jobs.rs` migrations/atomic/retry, subprocess/TTY `node/tui.rs` (201/201, 19/19 upstream wire fixtures; +12 tests; mutation-verified) |
| **Phase 1 — 1.5 (`pi-rs-p2-tui` & `hosts/tui`)** | **PASS** | `F1` CommandPalette, `/hotkeys`, `/settings` + `KeybindingsOverlay` + `prefs.json`, `/quit`, `/reload`, `Ctrl+C`/`Escape`, multiline `Editor` + completion, `$EDITOR`/browser actions, OSC 10/11/4 + DA1 + mode 2031 probe, `--radius` OAuth/WS bridge, live `TranscriptComponent`/tool renderers/`ShellRenderer`, 20 interactive plugin facets (51/51 TUI tests; 3/3 mutations) |
| **Phase 1 — 1.6 (`pi-rs-p2-web`)** | **PASS** | Full Wave 1.6a–1.6c browser UI, activity model, command menu keys, markdown, offline conversation cache, browser plugins, Chord bridge, and Leptos CSR WASM parity (173/173; 3/3 mutations) |
| **Phase 1 — Full Integrated Workspace & Packaged E2E** | **PASS** | `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings` (0 warnings), `cargo clippy --target wasm32-unknown-unknown -p pi-rs-p2-web --lib --locked -- -D warnings` (0 warnings), `cargo test --workspace --locked -- --test-threads=2` (**220 suites, 2,691 passed, 0 failed, 2 ignored**, 1,293.25s summed test time), `browser_live_e2e` (3/3 Chromium E2E), `./scripts/package-p2.sh --locked` PASS |

## Key Links

- Plan & history: [MAP.md](MAP.md) | Decisions: [DECISIONS.md](DECISIONS.md) | Glossary: [GLOSSARY.md](GLOSSARY.md) | Workspace: [Cargo.toml](../Cargo.toml) | Packaging: [package-p2.sh](../scripts/package-p2.sh)

## Known Hazards & Remaining External-Environment Items

1. **Build cache & temp disk**: Point `CARGO_TARGET_DIR` to raw ext4 (e.g. `/var/tmp`) with `CARGO_BUILD_JOBS=2 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0`. Avoid `/tmp` if `/tmp/.git` exists (breaks `pi-rs-durable` `local_execution_environment_conformance`) and avoid long `TMPDIR` paths for `pi-rs-p2-mac` Unix sockets (`SUN_LEN` 108-byte limit); e.g. `TMPDIR=/dev/shm/pi-rs-test`.
2. **Packaged `p2` builds**: Run `./scripts/package-p2.sh [--release] [--locked]` (`wasm-bindgen` CLI version must match `Cargo.lock`, currently `0.2.130`).
3. **Chord wire indexing**: Remote Chord links MUST index state/method/event members using `Contract::variant_member_names` (sorted by UTF-16), NOT declaration-order `member_names`.
4. **Remaining external/non-Linux items (Group F)**:
   - `pi-rs-p2-mac`: live macOS AppKit/`WKWebView`, tray, and signing (deferred per ADR-005; compile-checked via CI matrix).
   - `pi-rs-env` / `pi-rs-tui` / `pi-rs-coding-agent`: live macOS and Windows runtime verification (compile-checked via CI matrix).
   - `pi-rs-coding-agent`: live OAuth sign-in against external third-party identity providers with real user accounts (loopback PKCE + real Chromium verified).
