# `pi-rs` Handoff

- **Checkpoint**: Groups **A–E** parity, polish, and external-adapter verification closed and merged onto `main` on top of the lifted 18-crate root Cargo workspace (ADR-012) and zero-Node CI/packaging (`scripts/package-p2.sh`, `.github/workflows/{ci,release}.yml`).
- **Stage**: Pure-Rust standalone monorepo with all 18 crates, all 8 binaries (`pi`, `pi-ai`, `pi-env`, `pi-evals`, `p3`, `p2`, `pi-mac`, `pi-voice-server`), all 49 built-in `p2` plugins, `p2 tui`, packaged Leptos WASM browser UI, and Linux external adapters (`ort` Pocket TTS, `sshd`, OCI `docker`/`podman`, headless Chromium OAuth PKCE) verified.
- **Fallback rate**: 0% fallback; zero runtime or build dependency on Node.js, Swift, or Python.
- **Next action**: Only Group **F** remains: non-Linux native runtime verification (macOS AppKit/`WKWebView`/signing and Windows live runtime) plus optional live third-party identity-provider OAuth sign-in.

## Gate Table

| Gate | Status | Evidence |
| --- | --- | --- |
| Alignment, ADRs (`ADR-001`..`ADR-012`), primary-source research (8/8) | **PASS** | [DECISIONS.md](DECISIONS.md); all 8 notes in `rust/research/` |
| Root 18-crate Cargo workspace + zero-Node packaging & CI | **PASS** | `Cargo.toml`, `crates/pi-rs-*`, `scripts/package-p2.sh`, `.github/workflows/{ci,release}.yml`; `fmt`, workspace + `wasm32` `clippy -- -D warnings`, and packaged E2E PASS |
| **Group A** (`p2` Web UI Parity) | **PASS** | Attachment uploads (`p2.session-files`/`p2.attachments`), `localStorage` offline transcript cache + delta reconciliation, dynamic plugin hot-reload + draft preservation, WebKit/`pi-mac` shell theme bridge, and two-profile Chromium guest-share expiration E2E (`pi-rs-p2-web` 158/158, `browser_assets` 5/5, `browser_live_e2e` 3/3; 4/4 mutations verified) |
| **Group B** (`p2` Server, Plugins & TUI Polish) | **PASS** | `ports` `SessionArchived` immediate forward cleanup, combined mouse-scroll + live `/theme` PTY E2E against integrated worker, `--dev` dynamic rebuild/reload + `--dev-supervisor` restart (`pi-rs-p2-tui` 23/23, `pi-rs-p2` 18/18; 3/3 mutations verified) |
| **Group C** (`pi` CLI / `coding-agent` Parity) | **PASS** | Single-server MCP reconnect, startup background catalog refresh + missing configured package auto-install, loopback session-share & redacted bug-report E2E, Kitty/iTerm2 terminal images + Mermaid subgraph/width-fallback (`pi-rs-coding-agent` 886/886; 4/4 deliverables mutation-verified) |
| **Group D** (`p3` & `chord` Platform Seams) | **PASS** | `p3` address-level Chord worker routing (`IpcPipeLink` + `SessionRouter`), `pi-rs-chord` `wss://` TLS client (`WebSocketTlsConfig` + loopback TLS test), two-phase `ReloadCoordinator`, and journaled crash-safe `ScopedStoreSwap` (`pi-rs-chord` 50/50 binaries, `pi-rs-p3` 80/80; 5/5 mutations verified) |
| **Group E** (External Adapters) | **PASS** | **E1** `pi-rs-voice-server` Pocket TTS `ort` ONNX adapter + pure-Rust `.safetensors` loader (27/27 offline + 8/8 real `libonnxruntime.so.1.31.1` E2E; +12 tests); **E2** `pi-rs-env` native `packaged_daemon` + live ephemeral `sshd` E2E (51/51; +5 tests); **E3** `pi-rs-evals` OCI `docker`/`podman` runtime + live rootless `podman` E2E (51/51; +7 tests); **E4** `pi-rs-coding-agent` hardened browser launch seam + headless Chromium loopback OAuth 2.1 PKCE E2E (3/3 Chromium E2E; +8 tests); 13/13 mutations verified |

## Key Links

- Plan & history: [MAP.md](MAP.md) | Decisions: [DECISIONS.md](DECISIONS.md) | Glossary: [GLOSSARY.md](GLOSSARY.md) | Workspace: [Cargo.toml](../Cargo.toml) | Packaging: [package-p2.sh](../scripts/package-p2.sh)

## Known Hazards & Remaining External-Environment Items

1. **Build cache & temp disk**: If `/tmp` is a small tmpfs, point `CARGO_TARGET_DIR` and `TMPDIR` to disk-backed storage (e.g., under `/var/tmp`) with `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0`. Avoid long `TMPDIR` paths when running `pi-rs-p2-mac` Unix-socket tests (`SUN_LEN` 108-byte limit).
2. **Packaged `p2` builds**: Run `./scripts/package-p2.sh [--release] [--locked]` to build the pure-Rust CSS/font/WASM bundle (`wasm-bindgen` version must match `Cargo.lock`) and embed it via `P2_BROWSER_ASSETS`.
3. **Chord wire indexing**: Remote Chord links MUST index state/method/event members using `Contract::variant_member_names` (sorted by UTF-16), NOT declaration-order `member_names`.
4. **Remaining external/non-Linux items (Group F)**:
   - `pi-rs-p2-mac`: live macOS AppKit/`WKWebView`, tray, and signing (deferred per ADR-005; compile-checked via CI matrix).
   - `pi-rs-env` / cross-platform crates: live macOS and Windows runtime verification (compile-checked via CI matrix).
   - `pi-rs-coding-agent`: live OAuth sign-in against external third-party identity providers with real user accounts (loopback PKCE + real Chromium verified).
