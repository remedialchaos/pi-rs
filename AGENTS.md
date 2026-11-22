# Development Rules (`pi-rs`)

`pi-rs` (`remedialchaos/pi-rs`) is a 100% Rust rewrite of the `earendil-works/pi` monorepo (forked at v1.6.0, commit `780bf31fc`).

## Session Start

Before planning or editing code, read:

1. [rust/HANDOFF.md](rust/HANDOFF.md) — current milestone, gate table, and hazards.
2. [rust/MAP.md](rust/MAP.md) — full port plan, active claims, and dependency research queue.
3. [rust/DECISIONS.md](rust/DECISIONS.md) — binding ADRs (ADR-001 through ADR-009 and any later ADRs).
4. [rust/GLOSSARY.md](rust/GLOSSARY.md) — canonical crate, binary, and domain symbol names.

When porting units or wiring front doors, follow the `rust-anything` skill (`SKILL.md` and `whole-app.md`).

## Conversational Style

- Keep answers short and concise.
- No emojis in commits, issues, PR comments, docs, or code.
- No fluff or filler text. Technical prose only; be direct.
- Use clear, simple language; define unavoidable jargon before using it.
- Explain non-trivial designs and problems as: problem, concrete example or short trace, then solution. State why the solution is necessary and distinguish it from optional complexity.
- When the user asks a question, answer it first before making edits or running implementation commands.
- When responding to user feedback or an analysis, explicitly state whether you agree or disagree before describing changes.

## Strangler & Compatibility Rules

- **Upstream trees are read-only reference**: Do not edit files under `packages/` or `apps/` unless the user explicitly asks (or when wiring a temporary `rust-anything` shadow module per `whole-app.md`). All new Rust code, research notes, recorded cases, and port tracking live under `rust/`.
- **Drop-in compatibility (ADR-003)**: Match upstream external contracts strictly (CLI flags, exit codes, `~/.pi/agent/` files, JSONL v3 sessions, `--mode json` and `--mode rpc` framing, HTTP routes, SQLite schemas) until full parity is reached. Normalize only verified run-to-run noise in recorded cases, never in ports.
- **Research before porting (ADR-007)**: Before porting a package that relies on an unverified crate replacement, write its primary-source research note in `rust/research/<topic>.md` and read it before coding.
- **Claim before editing**: Claim each unit in `rust/MAP.md` before starting work, and record `gate` and `diff`/`replay` results when marking it done.
- **Checkpoints (ADR-009)**: Pause at each package/wave milestone, run `rust-anything map --write` (once crates exist), and rewrite `rust/HANDOFF.md` (<150 lines).

## Rust Code Quality

- Read referenced upstream source files and target Rust files in full before making wide-ranging changes or auditing behavior.
- Stable Rust, **edition 2024**.
- Crate naming: `pi-rs-*` (`publish = false` initially); preserve drop-in binary names (`pi`, `p3`, `p2`, `pi-env`) via `[[bin]]` targets (ADR-006).
- **Errors**: Where upstream throws or returns an error, return `Result<_, E>` (typed error enums in library crates). Never use `unwrap()`, `expect()`, or `panic!` on fallible paths. Only use them for invariants upstream also treats as impossible, accompanied by a `// INFALLIBLE: <why>` comment.
- **No `unsafe`**: Avoid `unsafe`. If genuinely required (e.g., OS TTY/clipboard/signal FFI), isolate it in a small platform module with a `// SAFETY:` comment explaining the invariant.
- **Lints**: Never suppress warnings with `#[allow(...)]`; use `#[expect(lint, reason = "...")]` so stale suppressions are caught.
- **Cross-platform hygiene (ADR-005)**: Linux `x86_64` is the primary dev/test target and Linux `aarch64` is secondary; keep macOS and Windows portable behind clean `#[cfg(...)]` boundaries without breaking compilation on non-Linux targets.
- **Semantic traps when porting from TS**:
  - UTF-16 code-unit offsets vs Rust UTF-8 byte offsets (`src/utf16.rs` when bridging JS indexes).
  - Map/object insertion order (`indexmap::IndexMap`) wherever key order reaches serialized or rendered output.
  - JS regex semantics (`\w`, `\b`, case folding, lookaround/backreferences -> `unicode(false)` or `fancy-regex` where needed).
  - JS numeric behavior (`NaN`, `-0.0`, integer ranges, float formatting).
- Inline single-line helpers with only one call site. Avoid speculative abstractions or premature frameworks.
- Keep all authored code 100% Rust (ADR-002, ADR-010): `wasmtime` hosting `quickjs.wasm` for `codemode`, `wasmtime` + WIT for dynamic extensions/Chord plugins, Rust-to-WASM for web UI.

## Verification Commands

For every Rust crate change:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

For `rust-anything` unit ports:

```bash
rust-anything build <crate>
rust-anything diff <crate>
rust-anything bench <crate>
rust-anything gate <crate>
```

For whole-app front doors:

```bash
rust-anything app replay <target>
```

For running the untouched upstream TS oracle (recording cases only):

```bash
npm install --ignore-scripts
```

- Never commit unless the user asks.
- For ad-hoc helper scripts, write them to `/tmp` or the session scratch directory and remove them when done; do not embed multi-line scripts in shell commands.

## Git

- Only commit files you changed in the current session, staged explicitly (`git add <path1> <path2>`).
- Before committing, check `git status` to verify only intended paths are staged.
- Commit message format: `{feat,fix,docs,refactor,chore}[(<crate-or-scope>)]: <concise summary>`.
- Never run `git reset --hard`, `git checkout .`, `git clean -fd`, `git stash`, `git add -A`, `git add .`, or `git commit --no-verify`.
- Never force push.
- Never include personal real-name metadata in repo files; use the maintainer's GitHub handle (`remedialchaos`) when attribution is needed.

## User Override

If the user's instructions conflict with any rule in this document, ask for explicit confirmation before overriding, then follow the user's instructions.
