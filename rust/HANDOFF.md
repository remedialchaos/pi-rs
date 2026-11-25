# `pi-rs` Handoff

- **Checkpoint**: Groups **A–D** parity and polish closed and merged onto `main` in alphabetical order on top of the lifted 18-crate root Cargo workspace (ADR-012) and zero-Node CI/packaging (`scripts/package-p2.sh`, `.github/workflows/{ci,release}.yml`).
- **Stage**: Pure-Rust standalone monorepo with all 18 crates, all 8 binaries (`pi`, `pi-ai`, `pi-env`, `pi-evals`, `p3`, `p2`, `pi-mac`, `pi-voice-server`), all 49 built-in `p2` plugins, `p2 tui`, and packaged Leptos WASM browser UI passing root workspace verification.
- **Fallback rate**: 0% fallback; zero runtime or build dependency on Node.js, Swift, or Python.
- **Next action**: Optional host/external-dependency items (Groups **E** & **F**): Pocket TTS ONNX/Candle synthesis adapter, live `sshd`/Docker runs, and native macOS/Windows runtime verification.

## Gate Table

| Gate | Status | Evidence |
| --- | --- | --- |
| Alignment, ADRs (`ADR-001`..`ADR-012`), primary-source research (8/8) | **PASS** | [DECISIONS.md](DECISIONS.md); all 8 notes in `rust/research/` |
| Root 18-crate Cargo workspace + zero-Node packaging & CI | **PASS** | `Cargo.toml`, `crates/pi-rs-*`, `scripts/package-p2.sh`, `.github/workflows/{ci,release}.yml`; `fmt`, workspace + `wasm32` `clippy -- -D warnings`, and packaged E2E PASS |
| **Group A** (`p2` Web UI Parity) | **PASS** | Attachment uploads (`p2.session-files`/`p2.attachments`), `localStorage` offline transcript cache + delta reconciliation, dynamic plugin hot-reload + draft preservation, WebKit/`pi-mac` shell theme bridge, and two-profile Chromium guest-share expiration E2E (`pi-rs-p2-web` 158/158, `browser_assets` 5/5, `browser_live_e2e` 3/3; 4/4 mutations verified) |
| **Group B** (`p2` Server, Plugins & TUI Polish) | **PASS** | `ports` `SessionArchived` immediate forward cleanup, combined mouse-scroll + live `/theme` PTY E2E against integrated worker, `--dev` dynamic rebuild/reload + `--dev-supervisor` restart (`pi-rs-p2-tui` 23/23, `pi-rs-p2` 18/18; 3/3 mutations verified) |
| **Group C** (`pi` CLI / `coding-agent` Parity) | **PASS** | Single-server MCP reconnect, startup background catalog refresh + missing configured package auto-install, loopback session-share & redacted bug-report E2E, Kitty/iTerm2 terminal images + Mermaid subgraph/width-fallback (`pi-rs-coding-agent` 886/886; 4/4 deliverables mutation-verified) |
| **Group D** (`p3` & `chord` Platform Seams) | **PASS** | `p3` address-level Chord worker routing (`IpcPipeLink` + `SessionRouter`), `pi-rs-chord` `wss://` TLS client (`WebSocketTlsConfig` + loopback TLS test), two-phase `ReloadCoordinator`, and journaled crash-safe `ScopedStoreSwap` (`pi-rs-chord` 50/50 binaries, `pi-rs-p3` 80/80; 5/5 mutations verified) |

## Key Links

- Plan & history: [MAP.md](MAP.md) | Decisions: [DECISIONS.md](DECISIONS.md) | Glossary: [GLOSSARY.md](GLOSSARY.md) | Workspace: [Cargo.toml](../Cargo.toml) | Packaging: [package-p2.sh](../scripts/package-p2.sh)

## Known Hazards & Remaining External-Environment Items

1. **Build cache & temp disk**: If `/tmp` is a small tmpfs, point `CARGO_TARGET_DIR` and `TMPDIR` to disk-backed storage (e.g., under `/var/tmp`) with `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0`. Avoid long `TMPDIR` paths when running `pi-rs-p2-mac` Unix-socket tests (`SUN_LEN` 108-byte limit).
2. **Packaged `p2` builds**: Run `./scripts/package-p2.sh [--release] [--locked]` to build the pure-Rust CSS/font/WASM bundle (`wasm-bindgen` version must match `Cargo.lock`) and embed it via `P2_BROWSER_ASSETS`.
3. **Chord wire indexing**: Remote Chord links MUST index state/method/event members using `Contract::variant_member_names` (sorted by UTF-16), NOT declaration-order `member_names`.
4. **Remaining external/non-Linux items (Groups E & F)**:
   - `pi-rs-voice-server`: Pocket TTS native ONNX/Candle synthesis adapter (Supertonic, Kokoro, Silero, and Parakeet are verified).
   - `pi-rs-env`: native `packaged_daemon()` layout & live `sshd`/macOS/Windows runtime verification.
   - `pi-rs-evals`: live Docker daemon run (offline injectable runner verified).
   - `pi-rs-p2-mac`: live macOS AppKit/`WKWebView`, tray, and signing (deferred per ADR-005; compile-checked via CI matrix).
   - `pi-rs-coding-agent`: live external OAuth browser sign-in against third-party identity providers.
