# `pi-rs` Handoff

- **Checkpoint**: **Phase 2 (`v1.6.1`, `780bf31fc..ef4a1cfcc`, all 71 non-merge commits)** complete and verified across all 18 crates on `main` (merged from `phase2/ai`, `phase2/core-libs`, `phase2/coding-agent`, `phase2/apps`, plus `1.6.1` runtime version sync and `pi-rs-durable` test initializer fix).
- **Stage**: Pure-Rust standalone monorepo with all 18 crates, all 8 binaries (`pi`, `pi-ai`, `pi-env`, `pi-evals`, `p3`, `p2`, `pi-mac`, `pi-voice-server`), all 49 built-in `p2` plugins, full `p2 tui` + `p2` browser UI parity, native Rust `pi-rs-evals` OCI harness, Linux external adapters, and full `v1.6.1` parity verified.
- **Fallback rate**: 0% fallback; zero runtime or build dependency on Node.js, Swift, or Python.
- **Next action**: Push `main` (after `airlock-review` when ready), then begin **Phase 3 (post-`v1.6.1` `main`, `ef4a1cfcc..ede8bdca0`)** starting with Workstream 3.1 (independent core updates) ahead of the breaking Workstream 3.2 `pi-rs-chord` grantable-members migration.

## Gate Table

| Gate | Status | Evidence |
| --- | --- | --- |
| Alignment, ADRs (`ADR-001`..`ADR-012`), primary-source research (8/8) | **PASS** | [DECISIONS.md](DECISIONS.md); all 8 notes in `rust/research/` |
| Root 18-crate Cargo workspace + zero-Node packaging & CI | **PASS** | `Cargo.toml`, `crates/pi-rs-*`, `scripts/package-p2.sh`, `.github/workflows/{ci,release}.yml` |
| Groups **A–E** baseline parity & Linux external adapters | **PASS** | Merged onto `main` at `69ccbbc42` |
| **Phase 1 — `v1.6.0` (`780bf31fc`) Full Parity & Post-ADR-012 Remediation (1.1a–1.7)** | **PASS** | Landed on `main` at `bfcce0595` (220 suites, 2,691 passed, 0 failed, 2 ignored) |
| **Phase 2 — P2-A (`pi-rs-ai`, §2.1)** | **PASS** | `phase2/ai` (`11495a41b`): `pi-decision` API (`POST /decision`) + Radius, Copilot `gpt-6-luna-decisions` + account filter, OpenRouter `responseId`/billed cost/`native_finish_reason` retries, Gemini 4 `xhigh` + `blockReason`, Responses `status: "completed"` + Codex pinned effort, retry classifier + `onResponse` 4xx/5xx, whole-identifier `request_too_large`, Anthropic `$schema`, Workers AI `""`, linear SSE, catalog refresh (65/65 tests, +14; 2/2 mutations) |
| **Phase 2 — P2-B (`pi-rs-mcp`, `pi-rs-tui`, `pi-rs-agent`, `pi-rs-env`, `pi-rs-durable`, §2.2–§2.3)** | **PASS** | `phase2/core-libs` (`bbbdf7819`, verified on Linux `aarch64`): MCP stringified numeric IDs, linear-time streaming across all 5 crates, TUI unclosed `$` Markdown, Tabby Windows paste skip, OSC 8 8,192-char cap, `tui-main-screen` crash-dump failure resilience, durable Windows Git Bash `git var GIT_SHELL_PATH` (459/459 tests on `aarch64`, +20; 10/10 mutations) |
| **Phase 2 — P2-C (`pi-rs-chord`, `pi-rs-p2`, `pi-rs-p2-web`, `pi-rs-p2-tui`, `pi-rs-p3`, §2.5)** | **PASS** | `phase2/apps` (`b756a4cf7`): backup plugin/user scope/streaming/paused/remote-empty restart, sync writer epochs removed, login away-timeout preservation, `cmd+shift` session cycle, subagent top-bar Archive, hidden transcript measurement stop, adaptive voice barge-in + headphones EC, WebKit image thumbnails, Markdown artifact scroll preservation, `p3` sync export + backup marker, linear `discord`/`terminals`/`chord` streaming, `wl-copy` MIME, `p2-tui` Mermaid note (+23 tests; `browser_live_e2e` PASS; `wasm32` Clippy PASS; 5/5 mutations) |
| **Phase 2 — P2-D (`pi-rs-coding-agent`, §2.4)** | **PASS** | `phase2/coding-agent` (`0e55b5047`): per-notice `notices.*` settings (default off), bash `full_output_error`/`fullOutputError` + temp-file resilience, `blockedKind` + OSC 7501 startup permission status, simplified first-time setup, `mistral-medium-latest`, `wl-copy -t text/plain`, oversized Mermaid explanation, unresolved `!command` `/login` status, WSL Windows `pi update` refusal, rebuilt `!` `ctrl+o`, Windows Git Bash `git var GIT_SHELL_PATH`, linear RPC/session streaming, docs/changelog sync (886+ unit + all integration suites, +9 tests; 2/2 mutations) |
| **Phase 2 — Full Integrated Workspace & Packaged E2E (`v1.6.1`)** | **PASS** | `phase2/v1.6.1` (`1.6.1` runtime version sync + `pi-rs-durable` `ClassifierResult.response_id`): `cargo fmt --all -- --check` PASS, `cargo clippy --workspace --all-targets --locked -- -D warnings` (0 warnings), `cargo clippy --target wasm32-unknown-unknown -p pi-rs-p2-web --lib --locked -- -D warnings` (0 warnings), `./scripts/package-p2.sh --locked` PASS, `cargo test --workspace --locked -- --test-threads=2` (**220 suites, 2,757 passed, 0 failed, 2 ignored**, +66 net tests, 1,529.52s summed test time) |

## Key Links

- Parity plan: [PARITY.md](../PARITY.md) | Port history: [MAP.md](MAP.md) | Decisions: [DECISIONS.md](DECISIONS.md) | Glossary: [GLOSSARY.md](GLOSSARY.md) | Workspace: [Cargo.toml](../Cargo.toml) | Packaging: [package-p2.sh](../scripts/package-p2.sh)

## Known Hazards & Remaining External-Environment Items

1. **Build cache & temp disk**: Point `CARGO_TARGET_DIR` to raw ext4 (e.g. `/var/tmp`) with `CARGO_BUILD_JOBS=2 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0`. Avoid `/tmp` if `/tmp/.git` exists (breaks `pi-rs-durable` `local_execution_environment_conformance`) and avoid long `TMPDIR` paths for `pi-rs-p2-mac` Unix sockets (`SUN_LEN` 108-byte limit); e.g. `TMPDIR=/dev/shm/pi-rs-test`.
2. **Packaged `p2` builds**: Run `./scripts/package-p2.sh [--release] [--locked]` (`wasm-bindgen` CLI version must match `Cargo.lock`, currently `0.2.130`).
3. **Chord wire indexing**: Remote Chord links MUST index state/method/event members using `Contract::variant_member_names` (sorted by UTF-16), NOT declaration-order `member_names` (until Phase 3 grantable-members migration).
4. **Remaining external/non-Linux items (Group F)**:
   - `pi-rs-p2-mac`: live macOS AppKit/`WKWebView`, tray, and signing (deferred per ADR-005; compile-checked via CI matrix).
   - `pi-rs-env` / `pi-rs-tui` / `pi-rs-coding-agent`: live macOS and Windows runtime verification (compile-checked via CI matrix).
   - `pi-rs-coding-agent`: live OAuth sign-in against external third-party identity providers with real accounts (loopback PKCE + real Chromium verified).
