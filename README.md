# pi-rs

AI agent toolkit: unified LLM API, agent loop, TUI, coding agent CLI, rewritten 100% in Rust.

`pi-rs` is a fork of [earendil-works/pi](https://github.com/earendil-works/pi) (v1.6.0, commit `780bf31fc`, MIT license) with the goal of porting the entire monorepo to 100% Rust while preserving drop-in compatibility at external boundaries (CLI flags, `~/.pi/agent/` configuration, JSONL v3 sessions, `--mode json` and `--mode rpc` wire protocols, HTTP routes, and SQLite schemas).

## Status

Active rewrite in progress. Upstream TypeScript, Swift, and Python trees (`packages/`, `apps/`) remain in this repository untouched as the behavioral reference and test oracle until each target passes differential verification and is lifted to a standalone Rust crate or binary.

Tracking and architecture documents live under [`rust/`](rust/):

- [rust/MAP.md](rust/MAP.md) — phased port plan, crate/unit progress, and research queue
- [rust/DECISIONS.md](rust/DECISIONS.md) — Architecture Decision Records (ADR-001 through ADR-009)
- [rust/GLOSSARY.md](rust/GLOSSARY.md) — upstream-to-Rust crate, binary, and symbol map
- [rust/HANDOFF.md](rust/HANDOFF.md) — current checkpoint state and gates

## Architecture & Roadmap

The port proceeds bottom-up in three phases using Rust edition 2024:

### Phase 1 — Core `pi` CLI & Libraries

| Crate | Binary | Replaces | Role |
| --- | --- | --- | --- |
| `pi-rs-schema` | — | `packages/schema` | Zero-dep JSON Schema (drafts 2020-12 & 7) compiler, validator, and coercion |
| `pi-rs-telemetry` | — | `packages/telemetry` | Vendor-neutral spans, metrics, propagation, and trace rendering |
| `pi-rs-mcp` | — | `packages/mcp` | Standalone MCP client (`stdio`, `streamable-http`, `pipe`) and OAuth |
| `pi-rs-codemode` | — | `packages/codemode` | Sandboxed async JS execution via pure-Rust `wasmtime` hosting `quickjs.wasm` |
| `pi-rs-tui` | — | `packages/tui` | Differential terminal UI framework (main-screen & alt-screen, components, native clipboard) |
| `pi-rs-ai` | `pi-ai` | `packages/ai` | Unified multi-provider LLM, image, and classifier client across all upstream wire APIs |
| `pi-rs-agent` | — | `packages/agent` | Stateful agent loop, tool execution, steering/follow-up queues, event streaming |
| `pi-rs-coding-agent` | `pi` | `packages/coding-agent` | Interactive, print, JSON, and RPC coding agent CLI with built-in tools, sessions, compaction, and WASM/WIT extensions |

### Phase 2 — Platform Runtime

| Crate | Binary | Replaces | Role |
| --- | --- | --- | --- |
| `pi-rs-chord` | — | `packages/chord` | Multi-process reactive object runtime, SQLite store, links, HTTP, workers, and WASM/WIT hot-reloading plugins |
| `pi-rs-durable` | — | `packages/durable` | Durable conversation, task, and document runtime |
| `pi-rs-env` | `pi-env` | `packages/env` | Remote SSH execution environment client integrating the existing Rust `pi-env` daemon |
| `pi-rs-evals` | — | `packages/evals` | Evaluation harness |

### Phase 3 — Applications

| Crate | Binary | Replaces | Role |
| --- | --- | --- | --- |
| `pi-rs-p3` | `p3` | `apps/p3` | Single-user Pi server and CLI on Chord |
| `pi-rs-p2` | `p2` | `apps/p2` | Self-hosted server, TUI, and Rust-to-WASM web UI |
| `pi-rs-p2-mac` | `p2-mac` | `apps/p2-mac` | Rust macOS desktop shell |
| `pi-rs-voice-server` | `pi-voice-server` | `apps/p2/voice-server` | Rust WebSocket speech-to-text and text-to-speech service |

### Runtime Boundaries

- **Extensions & Chord plugins**: built-in extensions/plugins compile directly into Rust via traits; dynamic and hot-reloadable plugins run as WASM components (`wasm32-wasip2`) via `wasmtime`.
- **Codemode**: LLM-authored scripts execute in a `wasmtime`-hosted `quickjs.wasm` sandbox (ADR-010) with zero ambient filesystem, network, or timer access and hard heap/stack limits.
- **Web UI**: compiled from Rust to WASM, sharing `pi-rs-chord` protocol and schema types directly with the server.

## Targets

- **Primary dev/test target**: Linux `x86_64`
- **Secondary testable target**: Linux `aarch64`
- **Portable targets**: macOS (`aarch64`, `x86_64`) and Windows (`x86_64`, `aarch64`) are kept buildable and isolated behind platform `cfg` boundaries, with live runtime testing deferred.

## Development

Prerequisites: stable Rust (edition 2024), `cargo`, and (while the upstream TS oracle is active) Node.js >= 22.19 and `rust-anything`.

```bash
# Hydrate upstream TS oracle (read-only reference for rust-anything record/diff)
npm install --ignore-scripts

# Verify porting toolchain
rust-anything doctor

# Once Rust crates are scaffolded under rust/
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## Security & Permissions

Like upstream `pi`, the `pi` coding agent runs built-in tools with the permissions of the local user and process that launched it. If you need OS-level isolation for tool execution, run `pi` inside a container or VM. See [SECURITY.md](SECURITY.md) for the full threat model and private vulnerability reporting instructions.

## License

MIT — see [LICENSE](LICENSE). Forked from [earendil-works/pi](https://github.com/earendil-works/pi).
