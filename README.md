# pi-rs

A 100% Rust rewrite of [earendil-works/pi](https://github.com/earendil-works/pi) — minimal, extensible AI agent harness, unified multi-provider LLM API, terminal UI, durable runtime, and self-hosted `p2` server.

`pi-rs` is an independent fork of `earendil-works/pi` (forked at v1.6.0, commit `780bf31fc`, MIT license). It replaces the upstream TypeScript, Swift, and Python monorepo with an 18-crate Rust workspace compiling directly to 8 native binaries (`pi`, `pi-ai`, `pi-env`, `pi-evals`, `p3`, `p2`, `pi-mac`, `pi-voice-server`), while preserving drop-in compatibility at external boundaries.

## Why `pi-rs`

Pi's core design — a minimal, composable agent harness that adapts to your workflows instead of locking you into rigid opinions — stays intact. `pi-rs` rebuilds that entire stack in Rust for users who want native binaries, smaller runtime footprints, and tighter isolation boundaries:

- **Zero Node.js, npm, Python, or Swift runtime dependencies**: The coding agent, LLM client, TUI, MCP client, Chord object runtime, `p2` server, `p2` web UI, `pi-env` SSH daemon, `pi-evals`, and `pi-voice-server` compile natively with Cargo (Rust edition 2024). There is no `node_modules` tree and no package-manager lifecycle script surface.
- **Drop-in external compatibility**: Preserves upstream CLI flags, `~/.pi/agent/` configuration (`settings.json`, `auth.json`, `models.json`), JSONL v3 session files, `--mode json` and `--mode rpc` wire protocols, HTTP routes, and SQLite schemas. Existing sessions, skills, prompt templates, themes, and tool integrations carry over.
- **Capability-scoped WASM sandboxing**: `pi-rs-codemode` runs LLM-authored JavaScript inside `wasmtime`-hosted `quickjs.wasm` with no ambient filesystem, network, or timer access and explicit memory, stack, and fuel limits. Dynamic extensions and Chord plugins execute as sandboxed WebAssembly components (`wasm32-wasip2` + WIT), while built-in extensions and all 49 `p2` plugins compile natively into Rust.
- **Self-contained `p2` browser UI**: The web frontend (`pi-rs-p2-web`) is written in Rust (Leptos CSR compiled to WASM), shares Chord protocol and schema types directly with the server, bundles CSS and fonts in pure Rust (`p2-bundle-assets`, zero `esbuild`), and embeds directly into the `p2` binary.
- **Native local adapters**: Direct ONNX Runtime (`ort`) speech synthesis and recognition in `pi-voice-server` (Supertonic, Kokoro, Pocket TTS, Silero VAD, Parakeet STT — no Python), native SSH daemon deployment in `pi-env`, configurable Docker/Podman container execution in `pi-evals`, and hardened loopback OAuth 2.1 PKCE.

## Differences and Current Scope

`pi-rs` aims to be transparent about where it diverges from upstream and what has been verified live:

- **Independent fork**: Forked from `earendil-works/pi` at v1.6.0 (`780bf31fc`). This repository is maintained independently and is not affiliated with `earendil-works/pi` or `pi.dev` services.
- **Dynamic extensions and plugins**: Dynamic extensions and Chord plugins run as WASM components (`wasm32-wasip2`) rather than loading arbitrary Node/TypeScript `.ts` modules via `jiti`/`esbuild`. Built-in extensions and all 49 built-in `p2` plugins are compiled directly into Rust.
- **Platform support**:
  - **Linux `x86_64`**: Primary development and full end-to-end verification target (including PTY TUI, headless Chromium `p2` web UI and OAuth PKCE, ephemeral `sshd`, rootless `podman`, and `ort` ONNX Runtime tests).
  - **Linux `aarch64`**: Built and checked in CI; release artifacts target `aarch64-unknown-linux-gnu`.
  - **macOS (`aarch64`) & Windows (`x86_64`)**: Kept buildable behind platform `cfg` boundaries and compile-checked on every push in CI, but live macOS AppKit/`WKWebView`/signing and live Windows runtime verification are currently deferred (see [ADR-005](rust/DECISIONS.md)). Note that `pi-mac`'s supervisor path still targets the upstream `apps/p2` layout until native macOS verification is prioritized.

## Install

Install prebuilt native binaries on Linux or macOS (verifies `SHA256SUMS.txt`, installs into `~/.pi/agent/install`, and links binaries onto `PATH`):

```bash
curl -fsSL https://raw.githubusercontent.com/remedialchaos/pi-rs/main/install.sh | sh
```

On Windows (PowerShell):

```powershell
powershell -c "irm https://raw.githubusercontent.com/remedialchaos/pi-rs/main/install.ps1 | iex"
```

Common installer options (pass via `sh -s -- ...` or `PI_*` environment variables):

```bash
# Install only the `pi` coding agent CLI (skip p2, p3, pi-env, etc.)
curl -fsSL https://raw.githubusercontent.com/remedialchaos/pi-rs/main/install.sh | sh -s -- --core-only

# Pin a specific release tag and custom bin directory non-interactively
curl -fsSL https://raw.githubusercontent.com/remedialchaos/pi-rs/main/install.sh | sh -s -- --version v1.6.0 --bin-dir ~/.local/bin --yes

# Build and install directly from source (requires Rust/cargo; works before a release tag is published or on other architectures)
curl -fsSL https://raw.githubusercontent.com/remedialchaos/pi-rs/main/install.sh | sh -s -- --from-source --core-only
```

Alternatively, install `pi` directly from source with Cargo:

```bash
cargo install --git https://github.com/remedialchaos/pi-rs --locked pi-rs-coding-agent
```

Or download a platform archive and `SHA256SUMS.txt` directly from GitHub Releases once a `v*` release tag is published.

### Update and Uninstall

Re-run the installer to update, reinstall, or uninstall interactively, or pass `--uninstall`:

```bash
curl -fsSL https://raw.githubusercontent.com/remedialchaos/pi-rs/main/install.sh | sh -s -- --uninstall
```

On Windows:

```powershell
$env:PI_UNINSTALL = "1"; powershell -c "irm https://raw.githubusercontent.com/remedialchaos/pi-rs/main/install.ps1 | iex"
```

Uninstalling removes only managed `pi-rs` binaries and release directories. Your configuration, credentials, sessions, and packages in `~/.pi/agent/` are preserved.

## Quickstart

Start `pi` in the project directory you want to work in:

```bash
cd /path/to/project
pi
```

Inside `pi`:

1. Run `/login` to connect a supported subscription (OAuth) or store an API key in `~/.pi/agent/auth.json` (or set provider environment variables directly).
2. Run `/model` to choose a model, or configure custom endpoints in `~/.pi/agent/models.json`.
3. Reference files inline with `@path/to/file`, give `pi` a task, and inspect each tool call and diff.
4. Resume the most recent session for the current directory with `pi --continue`, or pick any saved session with `/resume`.

`pi` also supports non-interactive automation:

- **Print mode**: `pi -p "Summarize the test failures"` for one-shot scripting.
- **JSON event stream**: `pi --mode json` for structured machine consumption.
- **RPC mode**: `pi --mode rpc` for bidirectional programmatic control over stdio.

### Customizing Pi

Start with the lightest mechanism that fits your workflow:

| Need | Start with |
| --- | --- |
| Persistent instructions for a repository or folder | `AGENTS.md` (loaded automatically from the workspace and `~/.pi/agent/`) |
| Reusable prompt triggered from the `/` menu | Prompt templates (`~/.pi/agent/prompts/`) |
| Task-specific instructions and reference files | Skills (`~/.pi/agent/skills/`) |
| Terminal palette and styling | Themes (`~/.pi/agent/themes/`) |
| External tools and data sources | MCP servers (`pi mcp ...`) |
| Sandboxed custom tools, commands, or event handlers | WASM/WIT extensions (`wasm32-wasip2`) |

### Other Binaries in the Suite

- **`p2`**: Self-hosted server and worker runtime with all 49 built-in plugins, an embedded Leptos WASM browser UI, and `p2 tui` for terminal access.
- **`p3`**: Single-user Pi server and CLI built on `pi-rs-chord`.
- **`pi-ai`**: Standalone CLI (`help`, `list`, `login [provider]`) for the unified multi-provider LLM client.
- **`pi-env`**: Remote SSH execution environment daemon and client.
- **`pi-evals`**: Containerized evaluation harness supporting Docker and rootless Podman (`PI_EVALS_CONTAINER_BIN`).
- **`pi-voice-server`**: Local HTTP/WebSocket speech-to-text and text-to-speech server backed by ONNX Runtime (`ort`).
- **`pi-mac`**: Rust macOS companion shell (compile-checked on macOS; live macOS runtime verification deferred).

## Architecture

The workspace lives at root [Cargo.toml](Cargo.toml) with 18 crates under `crates/pi-rs-*`:

### Core `pi` CLI & Libraries

| Crate | Binary | Upstream Equivalent | Role |
| --- | --- | --- | --- |
| `pi-rs-schema` | — | `packages/schema` | Zero-dep JSON Schema (drafts 2020-12 & 7) compiler, validator, and coercion (`regress` for JS regex semantics) |
| `pi-rs-telemetry` | — | `packages/telemetry` | Vendor-neutral spans, metrics, propagation, and trace rendering |
| `pi-rs-mcp` | — | `packages/mcp` | Standalone MCP client (`stdio`, `streamable-http`, `pipe`) and OAuth |
| `pi-rs-codemode` | — | `packages/codemode` | Sandboxed async JS execution via pure-Rust `wasmtime` hosting `quickjs.wasm` |
| `pi-rs-tui` | — | `packages/tui` | Differential terminal UI framework (main-screen & alt-screen, components, native X11 clipboard, Kitty/iTerm2 images) |
| `pi-rs-ai` | `pi-ai` | `packages/ai` | Unified multi-provider LLM, image, and classifier client across upstream wire APIs and OAuth flows |
| `pi-rs-agent` | — | `packages/agent` | Stateful agent loop, tool execution, steering/follow-up queues, event streaming |
| `pi-rs-coding-agent` | `pi` | `packages/coding-agent` | Interactive, print, JSON, and RPC coding agent CLI with built-in tools, sessions, compaction, and WASM/WIT extensions |

### Platform Runtime

| Crate | Binary | Upstream Equivalent | Role |
| --- | --- | --- | --- |
| `pi-rs-chord` | — | `packages/chord` | Multi-process reactive object runtime, SQLite store, IPC/WebSocket/TLS links, HTTP, workers, and WASM/WIT plugins |
| `pi-rs-durable` | — | `packages/durable` | Durable conversation, task, and document runtime |
| `pi-rs-env` | `pi-env` | `packages/env` | Remote SSH execution environment client and native `pi-env` daemon |
| `pi-rs-evals` | `pi-evals` | `packages/evals` | Containerized evaluation harness (`docker` / `podman`) |

### Applications

| Crate | Binary | Upstream Equivalent | Role |
| --- | --- | --- | --- |
| `pi-rs-p3` | `p3` | `apps/p3` | Single-user Pi server and CLI on Chord |
| `pi-rs-p2` | `p2` | `apps/p2` | Self-hosted server, worker runtime, and all 49 built-in plugins |
| `pi-rs-p2-web` | — | `apps/p2/src/web` | Leptos CSR WASM browser frontend for `p2` (ADR-011) |
| `pi-rs-p2-tui` | — | `apps/p2/src/tui` | Terminal UI client library powering `p2 tui` |
| `pi-rs-p2-mac` | `pi-mac` | `apps/p2-mac` | Rust macOS companion shell (compile-checked; live macOS verification deferred) |
| `pi-rs-voice-server` | `pi-voice-server` | `apps/p2/voice-server` | Local WebSocket/HTTP speech-to-text and text-to-speech service (`ort`) |

Architecture records and port history live under [`rust/`](rust/):

- [rust/DECISIONS.md](rust/DECISIONS.md) — Architecture Decision Records (ADR-001 through ADR-012)
- [rust/MAP.md](rust/MAP.md) — full port record, crate verification evidence, and research notes
- [rust/GLOSSARY.md](rust/GLOSSARY.md) — upstream-to-Rust crate, binary, and domain symbol map
- [rust/HANDOFF.md](rust/HANDOFF.md) — current checkpoint state, gate table, and known hazards

## Development

Prerequisites:

- Stable Rust (edition 2024) and `cargo`
- `wasm32-unknown-unknown` target (`rustup target add wasm32-unknown-unknown`) and `wasm-bindgen-cli` matching the `wasm-bindgen` version pinned in [Cargo.lock](Cargo.lock) (for building the packaged `p2` browser UI)
- For the full Linux E2E test suite: `tmux`, `openssh-server` (`/usr/sbin/sshd`), Chromium (`/usr/bin/chromium`), and `docker` or rootless `podman` (for `pi-rs-evals` live container tests). Optional real ONNX Runtime tests in `pi-rs-voice-server` use `ORT_DYLIB_PATH` with `--include-ignored`.

```bash
# Formatting, lints, installer checks, and full 18-crate test suite
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --target wasm32-unknown-unknown -p pi-rs-p2-web --lib --locked -- -D warnings
./scripts/test-install.sh
cargo test --workspace --locked

# Build the pure-Rust browser asset bundle and packaged `p2` binary (zero Node/esbuild)
./scripts/package-p2.sh --release --locked
```

[scripts/package-p2.sh](scripts/package-p2.sh) runs `p2-bundle-assets` (a pure-Rust CSS and WOFF2 font bundler in `pi-rs-p2-web`), compiles `pi-rs-p2-web` to `wasm32-unknown-unknown`, invokes `wasm-bindgen`, verifies all 8 browser assets, and builds `p2` with `P2_BROWSER_ASSETS` embedded.

## Security & Permissions

Like upstream `pi`, the `pi` coding agent runs built-in tools (`read`, `write`, `edit`, `bash`, `powershell`) with the permissions of the local user account that launched it. It does not implement an in-process permission gate around shell commands; if you need OS-level isolation for untrusted repositories or unattended runs, run `pi` inside a container or VM.

Where `pi-rs` enforces explicit sandbox boundaries — `pi-rs-codemode` (`quickjs.wasm` inside `wasmtime`) and WASM/WIT dynamic extensions and Chord plugins — sandbox escapes are treated as security vulnerabilities. See [SECURITY.md](SECURITY.md) for the full threat model and private vulnerability reporting instructions.

## License & Attribution

MIT — see [LICENSE](LICENSE).

`pi-rs` is a fork of [earendil-works/pi](https://github.com/earendil-works/pi) (v1.6.0, commit `780bf31fc`), created by the upstream Pi authors. Third-party notices for vendored fonts and voice components are preserved alongside their crates (see [crates/pi-rs-voice-server/THIRD-PARTY.md](crates/pi-rs-voice-server/THIRD-PARTY.md)).
