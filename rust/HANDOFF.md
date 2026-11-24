# `pi-rs` Handoff

- **Checkpoint**: Full 18-crate port + all 49 `p2` plugins + interactive UI polish (remote TUI plugin delivery/hot-swap, built-in native TUI views, whole-screen TUI PTY visual parity against upstream, browser plugin UI wiring, Chord sorted wire-order fix, and packaged real-Chromium E2E) published on `main`.
- **Stage**: All 18 workspace crates, all 8 binaries (`pi`, `pi-ai`, `pi-env`, `pi-evals`, `p3`, `p2`, `pi-mac`, `pi-voice-server`), all 49 built-in `p2` plugins, `p2 tui`, and the packaged Leptos WASM browser UI are ported, integrated, and passing offline & live E2E gates.
- **Fallback rate**: 0% fallback; `packages/` and `apps/` remain untouched (`git diff 780bf31fc -- packages/ apps/` is empty).
- **Next action**: Optional CI/release automation, host-specific polish (macOS native AppKit/`WKWebView`, external live OAuth/SSH/Docker, Pocket TTS ONNX adapter), closing documented fidelity limits, or retiring upstream reference trees.

## Gate Table

| Gate | Status | Evidence |
| --- | --- | --- |
| Alignment, `doctor`, `analyze`, repo docs/license, oracle hydration | **PASS** | ADR-001..011 in [DECISIONS.md](DECISIONS.md); `git diff 780bf31fc -- packages/ apps/` empty; privacy scan 0 matches |
| Primary-source research (8 of 8 topics) | **PASS** | All 8 notes complete in `rust/research/` |
| Unified 18-crate Cargo workspace (`rust/crates/Cargo.toml`) | **PASS** | `cargo fmt --check` PASS; offline `cargo clippy --workspace --all-targets -- -D warnings` PASS (0 warnings); `cargo clippy --target wasm32-unknown-unknown -p pi-rs-p2-web --lib -- -D warnings` PASS |
| **1.1–1.8** Phase 1 (`schema`, `telemetry`, `mcp`, `codemode`, `tui`, `ai`, `agent`, `coding-agent`) | **PASS** | 19/19 leaf gates PASS; full workspace test suite PASS (`pi`, `pi-ai` bins) |
| **2.1** `pi-rs-chord` | **PASS** | 2/2 leaf gates (4,995/4,995); `remote_runtime` 7/7 PASS incl. sorted UTF-16 wire member/method index parity (`variant_member_names` matching upstream `contract.ts:296`, mutation-verified) |
| **2.2–2.4** `pi-rs-durable`, `pi-rs-env`, `pi-rs-evals` | **PASS** | Full workspace suites PASS (`pi-env`, `pi-evals` bins, in-process PTY transport) |
| **3.1** `pi-rs-p3` (`p3` bin) | **PASS** | Full workspace suite PASS (lib + live `p3` CLI + live child-worker runner) |
| **3.2-tui** `pi-rs-p2-tui` + `p2 tui` (Items 2 & 3) | **PASS** | `pi-rs-p2-tui` 22/22 PASS; `tui_remote_plugins` 3/3 PASS (`/__p2_tui` + live WIT hot-swap); `tui_native_e2e` 1/1 PASS (live native `p2` server + built-in `/help`, `/models`, `/tree`, `/cost`, `/status`, `/new`); `tui_screen_parity` 2/2 PASS (live upstream vs native `100x24` PTY whole-screen parity + `120x24`/`80x24` `3:2` split cell-grid parity, 2/2 mutations verified) |
| **3.2-web** `pi-rs-p2-web` + packaged `p2` (Item 1) | **PASS** | `pi-rs-p2-web` 142/142 PASS (built-in plugin UIs, tool/activity cards, canonical contracts); `wasm32` + `wasm-bindgen 0.2.130` + `esbuild` bundle PASS; `browser_assets` 5/5 PASS; `browser_live_e2e` 2/2 PASS in real headless Chromium under production CSP (unauth sign-in, owner auth, zero JS/CSP errors, WASM/CSS/5 WOFF2 fonts, New session, live native `p2 --worker` chat + `Echo:`, `Page.reload` SQLite persistence, live `Models`/`Settings`/`Cost`/`Tree` panels) |
| **3.2** `pi-rs-p2` (`p2` bin, all 49 plugins) | **PASS** | All 49 upstream plugins ported (48 facet plugins + `sync` library); Phase 3 cross-plugin seams wired; `p2_all_builtins_e2e` 2/2 PASS; all crate suites green |
| **3.3** `pi-rs-p2-mac` (`pi-mac` bin) | **PASS** | 16/16 Linux tests PASS on `main` + `aarch64-apple-darwin` type-check PASS |
| **3.4** `pi-rs-voice-server` (`pi-voice-server` bin) | **PASS** | 17/17 offline tests PASS + real Supertonic/Kokoro/Silero/Parakeet verified |

## Key Links

- Plan & claims: [MAP.md](MAP.md) | Decisions: [DECISIONS.md](DECISIONS.md) | Glossary: [GLOSSARY.md](GLOSSARY.md) | Leaf map: [PORT_MAP.md](PORT_MAP.md) | Workspace: [crates/Cargo.toml](crates/Cargo.toml)

## Known Hazards & Notes

1. **Workspace layout**: Cargo workspace root MUST stay at `rust/crates/Cargo.toml` so local `rust/pi-rs-*` proof crates stay outside Cargo discovery and ignored via `/pi-rs-*/` in `rust/.gitignore`.
2. **Build cache & temp disk**: If `/tmp` is a small tmpfs, point `CARGO_TARGET_DIR` and `TMPDIR` to disk-backed storage (e.g., under `/var/tmp`) with `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0`. Avoid long `TMPDIR` or deep worktree paths when running `pi-rs-p2-mac` Unix-socket tests (`SUN_LEN` 108-byte limit).
3. **Browser asset packaging**: Build `pi-rs-p2` with `P2_BROWSER_ASSETS=<dir>` (produced by `cargo build -p pi-rs-p2-web --target wasm32-unknown-unknown`, `wasm-bindgen --target web --no-typescript --out-name app`, and `esbuild`) to embed the live WASM browser UI.
4. **Chord wire indexing**: Remote Chord links MUST index state/method/event members using `Contract::variant_member_names` (sorted by UTF-16 per upstream `contract.ts:296`), NOT declaration-order `member_names`.
5. **Documented fidelity limits** (recorded per crate `MILESTONE.md`):
   - `pi-rs-coding-agent`: external live OAuth browser sign-in, startup background catalog refresh/auto-install, single-server MCP reconnect, and exact interactive pixel replay remain future polish.
   - `pi-rs-p2` `ports`: `SessionArchived` cleanup contribution awaits a keyed native-registry seam (archived-session sweep at open is active).
   - `pi-rs-env`: `packaged_daemon()` assumes npm `bin/` layout (`local_daemon()` is the native path); live `sshd`/Windows/macOS runtime untested on Linux host.
   - `pi-rs-evals`: offline Docker verification via injectable runner; no live Docker daemon run.
   - `pi-rs-p2-mac` & `pi-rs-voice-server`: macOS UI/signing deferred per ADR-005; Pocket TTS is a discovery/streaming hook.
6. **Git & privacy safety**: `packages/` and `apps/` untouched; zero personal real-name metadata or home paths in working tree or published history.
