# Contributing to pi-rs

`pi-rs` is a 100% Rust rewrite of [earendil-works/pi](https://github.com/earendil-works/pi) focused on reaching full drop-in parity across the monorepo.

## Before You Start

1. Read [rust/DECISIONS.md](rust/DECISIONS.md) for settled architectural decisions (ADR-001 through ADR-009).
2. Read [rust/MAP.md](rust/MAP.md) to see what is planned, claimed, or already ported.
3. Read [AGENTS.md](AGENTS.md) for coding standards, error handling, and workflow rules.

## Scope During the Port

- Keep changes focused on porting planned milestones cleanly without drifting from upstream external behavior (CLI flags, `~/.pi/agent/` files, JSONL v3 sessions, `--mode json`/`--mode rpc`, HTTP/SQLite contracts).
- Do not modify upstream TypeScript, Swift, or Python reference files under `packages/` or `apps/` unless required by a temporary `rust-anything` shadow step or explicitly agreed in an issue.
- You must understand any code you submit and be able to explain how it preserves or verifies upstream behavior.

## Verification

Before opening a pull request, ensure the relevant checks pass:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

For ported units verified against the upstream oracle:

```bash
rust-anything diff <crate>
rust-anything gate <crate>
```

## Issues

Use the GitHub issue templates for bug reports and proposals. Keep reports concise, concrete, and reproducible, including the affected crate or command and minimal reproduction steps.
