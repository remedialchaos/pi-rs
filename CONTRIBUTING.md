# Contributing to pi-rs

`pi-rs` is a 100% Rust rewrite of [earendil-works/pi](https://github.com/earendil-works/pi) focused on reaching full drop-in parity across the monorepo.

## Before You Start

1. Read [rust/DECISIONS.md](rust/DECISIONS.md) for settled architectural decisions (ADR-001 through ADR-012).
2. Read [rust/MAP.md](rust/MAP.md) to see the ported crates, verification evidence, and architecture history.
3. Read [AGENTS.md](AGENTS.md) for coding standards, error handling, and workflow rules.

## Scope & Compatibility

- Keep changes focused and preserve drop-in compatibility with upstream external contracts (CLI flags, `~/.pi/agent/` files, JSONL v3 sessions, `--mode json`/`--mode rpc`, HTTP/SQLite contracts).
- Keep the repository 100% Rust with zero Node.js, npm, or `esbuild` build dependencies (ADR-002, ADR-012).
- You must understand any code you submit and be able to explain how it preserves or verifies behavior.

## Verification

Before opening a pull request, ensure the relevant checks pass:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --target wasm32-unknown-unknown -p pi-rs-p2-web --lib --locked -- -D warnings
cargo test --workspace --locked
```

For packaged `p2` browser asset builds:

```bash
./scripts/package-p2.sh --locked
```

## Issues

Use the GitHub issue templates for bug reports and proposals. Keep reports concise, concrete, and reproducible, including the affected crate or command and minimal reproduction steps.
