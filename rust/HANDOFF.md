# `pi-rs` Handoff

- **Checkpoint**: Final lift complete (ADR-012) — upstream `packages/`, `apps/`, and legacy JS/TS/Nix trees retired; 18-crate Cargo workspace promoted to root `Cargo.toml` / `crates/pi-rs-*`; zero Node.js/npm/`esbuild` dependencies via pure-Rust `p2-bundle-assets` + `scripts/package-p2.sh`; GitHub Actions CI (`.github/workflows/ci.yml`) and release workflow (`.github/workflows/release.yml`) added.
- **Stage**: Pure-Rust standalone monorepo with all 18 crates, all 8 binaries (`pi`, `pi-ai`, `pi-env`, `pi-evals`, `p3`, `p2`, `pi-mac`, `pi-voice-server`), all 49 built-in `p2` plugins, `p2 tui`, and packaged Leptos WASM browser UI passing root workspace verification.
- **Fallback rate**: 0% fallback; zero runtime or build dependency on Node.js, Swift, or Python.
- **Next action**: Review and prioritize remaining parity and polish gaps (documented below).

## Gate Table

| Gate | Status | Evidence |
| --- | --- | --- |
| Alignment, ADRs (`ADR-001`..`ADR-012`), primary-source research (8/8) | **PASS** | [DECISIONS.md](DECISIONS.md); all 8 notes in `rust/research/` |
| Asset decoupling & zero-Node `p2` bundler (`p2-bundle-assets`) | **PASS** | Vendored `pi-rs-coding-agent` assets, `pi-rs-schema` fixtures, Elanor CSS/WOFF2/licenses; `p2-bundle-assets` mutation-verified; `scripts/package-p2.sh` builds packaged `p2` with zero Node/`esbuild` |
| Root 18-crate Cargo workspace (`Cargo.toml`, `crates/pi-rs-*`) | **PASS** | `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, `cargo clippy --target wasm32-unknown-unknown -p pi-rs-p2-web --lib --locked -- -D warnings`, and `cargo test --workspace --locked` PASS |
| **Phase 1** (`schema`, `telemetry`, `mcp`, `codemode`, `tui`, `ai`, `agent`, `coding-agent`) | **PASS** | Full root workspace tests PASS (`pi`, `pi-ai` bins) |
| **Phase 2** (`chord`, `durable`, `env`, `evals`) | **PASS** | Full root workspace tests PASS (`pi-env`, `pi-evals` bins) |
| **Phase 3** (`p3`, `p2`, `p2-web`, `p2-tui`, `p2-mac`, `voice-server`) | **PASS** | All 49 `p2` plugins, `tui_screen_parity` 2/2, `browser_assets` 5/5, real-Chromium `browser_live_e2e` 2/2, `p2-mac` Linux suite, and `pi-voice-server` suite PASS |
| CI & release automation (`.github/workflows/{ci,release}.yml`) | **PASS** | Lint, Linux x86_64 test + packaged `p2` E2E, cross-platform check (`ubuntu-24.04-arm`, `macos-latest`, `windows-latest`), and tag release workflow validated |

## Key Links

- Plan & history: [MAP.md](MAP.md) | Decisions: [DECISIONS.md](DECISIONS.md) | Glossary: [GLOSSARY.md](GLOSSARY.md) | Workspace: [Cargo.toml](../Cargo.toml) | Packaging: [package-p2.sh](../scripts/package-p2.sh)

## Known Hazards & Remaining Parity Gaps

1. **Build cache & temp disk**: If `/tmp` is a small tmpfs, point `CARGO_TARGET_DIR` and `TMPDIR` to disk-backed storage (e.g., under `/var/tmp`) with `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0`. Avoid long `TMPDIR` paths when running `pi-rs-p2-mac` Unix-socket tests (`SUN_LEN` 108-byte limit).
2. **Packaged `p2` builds**: Run `./scripts/package-p2.sh [--release] [--locked]` to build the pure-Rust CSS/font/WASM bundle (`wasm-bindgen` version must match `Cargo.lock`) and embed it via `P2_BROWSER_ASSETS`.
3. **Chord wire indexing**: Remote Chord links MUST index state/method/event members using `Contract::variant_member_names` (sorted by UTF-16), NOT declaration-order `member_names`.
4. **Remaining parity & polish gaps** (to prioritize next):
   - `pi-rs-coding-agent`: external live OAuth browser sign-in, startup background catalog refresh/auto-install, single-server MCP reconnect, session-share uploads, bug-report delivery, terminal image protocols, and exact interactive pixel/Mermaid replay.
   - `pi-rs-p2` / `pi-rs-p2-web` / `pi-rs-p2-tui`: `ports` `SessionArchived` keyed-registry cleanup; web attachment uploads, offline transcript persistence, browser-side dynamic plugin hot-reload, WebKit/shell theme bridge, and guest share expiration E2E; TUI mouse-scroll + theme-change combined check and `--dev` dynamic rebuild/supervisor restart.
   - `pi-rs-p3`: wire worker links onto `pi-rs-chord`'s `2.1b-3` address-level worker routing.
   - `pi-rs-chord`: WSS client support, distributed WASM reload holdback, crash-atomic SQLite+filesystem store swap.
   - `pi-rs-voice-server`: Pocket TTS native ONNX/Candle synthesis adapter.
   - Host-specific live verification: macOS AppKit/`WKWebView` & signing (`pi-rs-p2-mac`), live `sshd`/Windows/macOS runtime (`pi-rs-env`), live Docker daemon (`pi-rs-evals`).
