# `pi-rs-coding-agent` milestones

## Milestone 1.8a — standalone foundation (ACCEPTED 2026-11-22)

Status: complete and verified. Scope: every `pi-ai`-independent subsystem of
`packages/coding-agent` needed before the session runtime and `pi` front door.
All authored code is 100% Rust, edition 2024, `publish = false`, standalone `[workspace]`.

### Leaf gates (Part A)

| Unit crate | Upstream unit | Recorded cases | Diff | Bench verdict | Gate |
| --- | --- | --- | --- | --- | --- |
| `rust/pi-rs-coding-agent-changelog` | `utils/changelog.ts` | 898 (19 from `changelog.test.ts`) | 898/898 | 0.23x — port with callers | PASS |
| `rust/pi-rs-coding-agent-frontmatter` | `utils/frontmatter.ts` | 230 (7 targeted inputs) | 230/230 | 1.64x — ship | PASS |
| `rust/pi-rs-coding-agent-ansi` | `utils/ansi.ts` | 711 (265 from `ansi-utils.test.ts`) | 711/711 | 0.08x — port with callers | PASS |

All three passed every gate check (fmt, clippy `-D warnings`, cargo test, unsafe, lockfile,
advisories, no `unimplemented!`, size, error cases, panics, build, API, coverage, behavior).

### Foundation verification

```
cargo fmt --check                            PASS
cargo clippy --all-targets -- -D warnings    PASS
cargo test                                   316 passed (0.49 s) at acceptance
```

### Foundation module inventory (316 tests at 1.8a)

- Foundation: `config` (9), `migrations` (7), `lockfile` (6) — proper-lockfile-compatible
  atomic `mkdir <target>.lock` with mtime staleness, realpath aliasing, retries.
- `utils/` (31 modules, 159 tests): abort, ansi, changelog, child_process, clipboard,
  clipboard_command, clipboard_image, exif_orientation, flush_stdout, frontmatter, fs_watch,
  git, html, image_convert, image_process, image_resize, json, management_http, mime,
  open_browser, output_files, paths, shell, sleep, syntax_highlight, terminal_question, text,
  tools_manager, version_check, web_url, wsl, zip.
- `core/` (135 tests): keybindings (13), skills (19), prompt_templates (21), project_trust (10),
  tools executors (72) — path_utils, truncate, edit_diff, find, grep, ls, read, write, edit,
  bash, powershell. No path sandbox (upstream accepts absolute paths and `..` by design).

---

## Milestone 1.8b-1 — core runtime, sessions, compaction, extensions & built-ins (2026-11-22)

Status: complete and verified. Built on all seven accepted Phase 1 crates.

### Verification (unified workspace update)

```
cargo fmt --all --check                             PASS
cargo clippy --workspace --all-targets -- -D warnings PASS
cargo test -p pi-rs-coding-agent                    759 passed, 0 failed (18.88 s)
git diff packages/ apps/                            empty
```

No `unsafe`, no `#[allow(..)]`, no `unwrap`/`expect`/`panic!` on fallible library paths
(lock poisoning uses `unwrap_or_else(|poisoned| poisoned.into_inner())`), no personal metadata
or absolute home paths. All 759 tests are offline: HTTP is closure-injected, `rg`/`fd` tests
skip when the binary is absent, and agent turns use `pi_rs_ai::faux::faux_provider`.

### Crate wiring

Member of the unified `rust/crates/Cargo.toml` workspace, wired directly to all seven Phase 1
crates (`pi-rs-schema`, `pi-rs-telemetry`, `pi-rs-mcp`, `pi-rs-codemode`, `pi-rs-tui`,
`pi-rs-ai`, `pi-rs-agent`) plus `wasmtime =50.0.2`. `pi-rs-tui`'s `libm =0.2.11` pin conflict
was resolved by inlining SunPro/FreeBSD `cbrt` in `pi-rs-tui/src/oklab.rs` (resolving cleanly
to `libm 0.2.16` workspace-wide). The extension host keeps injectable UI callbacks
(`select`/`confirm`/`input`/`notify`) for headless/offline testing while allowing 1.8b-2 to
bind directly to `pi-rs-tui`.

### Module inventory (758 tests)

**Sessions & persistence** — `core::session_manager` (39): JSONL v3 header + all 11 entry
kinds, v1→v2→v3 migration with rewrite, tree (`get_tree`/`get_branch`/`branch`/
`branch_with_summary`/`fork`/`navigate_tree`), projection, crash-safe append with
truncate-to-last-good-offset rollback, cwd-derived session directories, session listing.

**Settings, auth, models** — `settings_manager` (20), `auth_storage` (12),
`resolve_config_value` (13), `timings` (9), `model_registry` (14), `model_resolver` (24),
`messages` (11), `system_prompt` (10), `slash_commands` (7), `footer_data` (8),
`bash_executor` (8), `utils::pi_user_agent` (5), `utils::tool_result_images` (9).
Deep merge with `defaultTools` layering, trust-gated project writes, `0600` auth file with
`lockfile`, `$ENV`/`!command` resolution, npm-style scoped model matching and thinking
suffixes, trust-gated `SYSTEM.md`/`APPEND_SYSTEM.md`, 24 built-in slash commands.

**Compaction** — `core::compaction` (40): UTF-16 `chars/4` token estimate, cut points that
never split tool-call/result pairs, file-ops tracking, injectable summarizer, branch
summaries, details serialization, upstream prompt constants.

**AgentTool wrappers** — `core::tools::{wrappers,tool_definition_wrapper,renderers}` (32):
8 tools (`read`, `bash`, `powershell`, `edit`, `write`, `grep`, `find`, `ls`) implementing
`pi_rs_agent::AgentTool` with exact strict-mode schemas, image auto-resize, cancellation,
call/result renderers. Wraps the 1.8a pure executors (72 executor tests still present).

**Extension host** — `event_bus` (8), `extensions::{types,runner,loader,context,wasm}` (69),
`resource_loader` (9): all 41 events with first-winner/chain/last-override dispatch and
coalesced `ui_prompt_start/end`; first-wins tools, `name:1`/`name:2` commands, reserved
shortcut skipping, single-owner MCP servers, `Loading→Active/Failed` atomic transaction,
generation tokens, `/reload` nine-step coordinator, pre/post-trust loading, typed
`option<patch>` WASM boundary with Store invalidation.

**Built-in extensions** — top-level `extensions/` (51): `builtin:codemode` and `builtin:mcp`
(replaceable + hidden), `builtin:tool-search` (replaceable + hidden), `builtin:llama.cpp`
(not replaceable), `omit_replaced_extensions()` collision rule.

**Agent session runtime** — `agent_session` (16), `agent_session_runtime` (8), `sdk` (7):
orchestration over `pi_rs_agent::Agent` with auto-compaction, abortable exponential
auto-retry, steering/follow-up, branching/tree navigation, model/thinking switching,
`/reload`, statistics, and offline `InMemory*` service adapters.

**HTML export & package manager** — `export_html` (20), `package_manager` (16):
self-contained HTML with ANSI/OSC-8 links, `hljs-`-prefixed syntax classes, CJK emphasis,
XSS-safe escaping, deep-tree rendering; offline package source parsing, install plans,
lock contention, self-update command construction.

### Ready for Milestone 1.8b-2 / caveats

1. `cli/`, `modes/{print,json,rpc,interactive}/`, `main.ts`, and the `[[bin]] name = "pi"`
   front door are not in this milestone; all runtime seams are in place and `pi-rs-tui` is
   wired directly in `Cargo.toml`.
2. Checked-in WIT world `wit/extension.wit` (`package pi:extension@0.1.0`) is bound via
   `wasmtime::component::bindgen!` in `src/core/extensions/wasm.rs` and verified end-to-end
   against the on-disk WebAssembly Component fixture `tests/fixtures/echo_extension.wasm` in
   `tests/wasm_wit_extension.rs` (`init`, `on_tool_call`, `on_before_headers`, `execute_tool`
   including host callback delegation, `run_command`, and `/reload` `Store` drop + stale
   rejection). Full 41-event WIT surface expansion proceeds with 1.8b-2/2.1b as needed.
3. `syntax_highlight` maps syntect scopes to `hljs-`-prefixed classes; exact highlight.js
   v10 token boundaries remain a snapshot-parity caveat for HTML export.
4. Built-in MCP extension renders server/tool sections and names via `pi_rs_mcp`; live MCP
   transport wiring happens in 1.8b-2's front door.
5. `pi-rs-coding-agent` remains a library-only crate at this milestone (no `[[bin]]`).

---

## Milestone 1.8b-2a — native non-interactive `pi` (2026-11-22)

**Status:** Native front door and protocol implemented; full ADR-003 parity is not established.

- `cli/`: hand-written argument parsing and help; stdin and `@file` text/image assembly;
  model table; auth check, login, logout, and credential printing; project trust and setup.
- `main.rs`, `package_manager_cli.rs`, `utils/windows_self_update.rs`: mode and session
  selection, HTML export, package lifecycle and updates, and the deferred TUI entry point.
- `modes/`: text and JSON output, strict LF-framed RPC server and typed client, all 33 RPC
  commands, and extension UI requests and responses. The RPC reader keeps accepting
  commands while prompts, Bash commands, and extension dialogs are active.
- `core/`: on-disk settings, credentials, v3 sessions, provider and model dispatch,
  compaction, diagnostics, and the native extension host behind the front door.

### Verification

| Gate | Result |
| --- | --- |
| Scoped `cargo fmt -- --check` / offline `cargo clippy --all-targets -- -D warnings` | Passed |
| Scoped offline `cargo test` | 806 passed, 0 failed, 0 ignored |
| TypeScript oracle replay | 8/8: help, version, invalid mode, unknown option, RPC rejects `@file`, model list, package list, auth check |
| Loopback provider, TypeScript vs. Rust | Print bytes and all 15 LF-framed JSON event types matched; both sent the same stdin + `@file` + positional message |
| Actual interactive TTY | Printed the 1.8b-2b notice and exited 1 |
| Mutation checks | Selected new regressions failed after deliberate parser, output, auth, package, provider, and RPC breaks; source restored before final gates |

Added 47 tests; removed none. Previous milestone recorded 759 tests in 18.88 s.
This run reported 23.62 s for the unit suite and 27.99 s summed across test binaries,
excluding compilation. A fresh before-run did not fit in the `/tmp` tmpfs; timings
are not a controlled performance comparison.

### Parity limits

- Interactive TTY and the interactive resume/config selectors are reserved for 1.8b-2b.
  The TTY path prints a milestone notice and exits 1.
- The standalone upstream `pi mcp` subcommand is not ported. The in-session MCP command
  can inspect and reconnect servers but does not handle OAuth or expose MCP tools to the
  model. The in-process RPC UI is verified for native extensions; the existing WIT world
  does not expose guest dialogs or tool/provider registration.
- Startup uses cached model catalogs; it does not run the upstream background RPC catalog
  refresh or install missing configured packages. Explicit catalog updates use the native
  HTTP dispatcher; `pi-rs-ai` streaming does not use its proxy/idle-timeout settings.
- Help is unstyled, non-ASCII model sorting does not use ICU collation, and the native HTML
  renderer does not reproduce upstream markup byte for byte.

---

## Milestone 1.8b-2b — native interactive `pi` (2026-11-22)

The native `pi` front door starts `pi-rs-tui` in regular (scrollback-preserving)
or fullscreen mode. The interactive loop owns the editor, viewport, transcript,
footer, session transitions, slash commands, and native extension dialogs and
shortcuts. The CLI opens terminal selectors for resume, config, and startup trust.
Config edits persist resource rules used by subsequent runs.

The theme layer embeds dark/light, validates custom palettes against the upstream
schema, and watches palette files for changes. Transcript rendering handles
streamed assistant and tool updates, Markdown, images, diffs, native Mermaid, and
persisted Bash and session-summary rows.

### Verification

- Faux provider: regular/fullscreen rendering, streaming deltas, real read-tool
  output and expansion, model/thinking/theme changes, compaction, reload, new
  sessions, extension dialogs and shortcuts, and terminal cleanup.
- Linux PTY: main and alternate screens started and restored terminal state on
  exit. `--resume` selected a v3 session; `pi config` saved a prompt exclusion
  that a subsequent native RPC process respected.
- Mutation checks failed on the intended assertions for the optional theme
  token fallback, partial tool output, stale slash completion, footer thinking
  level, and extension confirmation. All five mutations were reverted.

| Gate | Result |
| --- | --- |
| Scoped `cargo fmt -- --check` | Passed |
| Scoped offline `cargo clippy --all-targets -- -D warnings` | Passed |
| Scoped offline `cargo test` | 865 passed, 0 failed, 0 ignored |

Added 59 tests, removed none. Recorded suite runtime before and after was
27.99 s → 26.81 s (sum across test binaries; compilation excluded). The unit
suite was 23.62 s → 21.75 s. This was not a controlled benchmark.

### Parity limits

- No captured TypeScript interactive replay was available. Pixel/frame parity,
  complex Mermaid layout, animation details, and the upstream staged login picker
  are not established. Unsupported or too-wide Mermaid falls back to source.
- Native commands and shortcuts have `ExtensionContext` UI callbacks. Guest WASM
  dialogs require a WIT change outside this milestone's scope. Extension
  provider/virtual-model queues are drained on reload but are not applied to the
  live AI registry.
- Live OAuth, session-share uploads, bug-report delivery, and terminal image
  protocols were not exercised end to end. Missing configured packages are not
  auto-installed at startup.
