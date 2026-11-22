# Primary-Source Research: `wasmtime` + WIT Extensions & Chord Plugins

Covers Milestone 1.8 (`packages/coding-agent/src/core/extensions/`) and Milestone 2.1 (`packages/chord/src/node/plugins/` + `src/host/`). Every claim cites upstream source or `docs.rs/wasmtime`.

## 1. Crate Choice: `wasmtime` Component Model + Async WIT (`docs.rs/wasmtime`)

Per ADR-002, built-in extensions and core plugins compile directly into Rust via host traits, while dynamically loaded / hot-reloadable extensions and Chord plugins compile to **`wasm32-wasip2`** components hosted by **`wasmtime`**:
- **`wasmtime::component::bindgen!({ path: "...", world: "...", async: true })`** generates async host traits and `instantiate_async(...)` bindings (`docs.rs/wasmtime/latest/wasmtime/component/macro.bindgen.html`).
- **Why async WIT is mandatory**: extension `init`, event handlers, `execute_tool`, `run_command`, `ctx.ui.{select,confirm,input,editor}`, `ctx.reload()`, `exec()`, and nested `ctx.execute_tool()` are async and re-enter the host (e.g., a tool calling `ctx.ui.confirm` or nested `ctx.execute_tool`).
- **Capability sandboxing**: WASI capabilities (filesystem preopens, network, subprocess `exec`) are explicitly opt-in per `Store` / `WasiCtx` instead of ambient process privileges (strictly stronger than upstream `jiti` and `p2`'s Node `--permission` worker).
- **Instance invalidation on `/reload`**: Each session/reload binds a fresh `wasmtime::Store` + generation token. Any call from an invalidated instance returns a stale-context error (`loader.ts#L200-L207,L265-L270`), and dropping the old `Store` reclaims guest memory, `EventBus` subscriptions, and resources cleanly.

## 2. Coding-Agent Extension Surface (`packages/coding-agent/src/core/extensions/`)

### Unified Rust Host Trait Split
Both compiled-in Rust built-in extensions (`llama.cpp`, `codemode`, `tool-search`, `mcp` — `extensions/index.ts#L7-L14`) and `WasmExtension` adapters wrapping a `wasmtime::component::Instance` implement a shared `Extension` trait against a shared `ExtensionHost` runtime so dispatch order, replaceability, conflict diagnostics, and `/reload` semantics stay identical.

### Built-in vs User-Loaded Semantics (`resource-loader.ts#L140-L181,L541-L638,L766-L841`)
- **Pre-trust vs post-trust**: Only global (`~/.pi/agent/extensions/`) and CLI `-e` extensions load before `project_trust` resolves (using restricted `ProjectTrustContext`: `cwd`, `mode`, `hasUI`, `ui: select|confirm|input|notify`; first `"yes"`/`"no"` wins). Project-local `.pi/extensions/` and `builtin:<name>` load after trust resolves.
- **Built-ins**: `builtin:llama.cpp` (`replaceable: false`), `builtin:codemode`, `builtin:tool-search`, `builtin:mcp` (`replaceable: true`, `hidden: true`). If any non-replaceable extension registers the same `tool:<name>`, `command:<name>`, or `flag:<name>`, `omitReplacedExtensions()` drops the replaceable built-in with a warning instead of erroring.
- **Pre-load transaction (`loader.ts#L164-L246`)**: Flag defaults, providers, and MCP server registrations queue during `init()` (`state == Loading`), commit atomically on `Ok`, and discard (plus unsubscribe `EventBus` listeners) on `Err`.

### Exact Event & Registration Contract (`types.ts#L600-L2036`, `runner.ts#L98-L1575`)
- **41 events** across Startup/Resources (`project_trust`, `resources_discover`, `mcp_servers_change`), Session (10: `session_start`, `session_info_changed`, `session_before_switch`, `session_before_fork`, `session_before_compact`, `session_compact`, `session_compact_failed`, `session_shutdown`, `session_before_tree`, `session_tree`), Agent/Turn/Provider/Message (18: `context`, `context_with_system`, `cache_warming_decision`, `before_provider_headers`, `before_provider_request`, `after_provider_response`, `provider_stream_event`, `before_agent_start`, `agent_start`, `agent_end`, `agent_before_settle`, `agent_settled`, `turn_start`, `turn_end`, `message_start`, `message_update`, `message_end`, `tool_execution_{start,update,end}`), and Model/Tool/Input/UI (10: `model_select`, `thinking_level_select`, `tool_call`, `tool_result`, `user_bash`, `input`, `ui_prompt_{start,end}`).
- **Dispatch rules**:
  - Short-circuit first-winner: `project_trust`, `session_before_switch` (`cancel`), `tool_call` (`block: true`), `user_bash`, `input` (`action: "handled"`).
  - Chained transform: `before_provider_request`, `message_end` (must preserve `role`), `tool_result` (replacing `content` without `structuredContent` drops `structuredContent`), `input` (`action: "transform"`).
  - Last override wins: `cache_warming_decision`.
  - Coalesced microtask lifecycle: `ui_prompt_start` / `ui_prompt_end` via `uiPromptDepth` around blocking UI calls (`runner.ts#L581-L626`).
- **Registrations**: `register_tool` (first registration per name wins; post-load calls `refresh_tools()`), `register_command` (duplicate names disambiguated as `name:1`, `name:2`), `register_shortcut` (reserved keybindings in `runner.ts#L98-L117` skipped with diagnostic), `register_flag` / `get_flag`, renderers (`message`, `entry`, `tool` middleware, `markdown` transformer), `register_provider` / `register_virtual_model`, `register_mcp_server` (enforces single owner + unique `mcpNamespace`).
- **Context hierarchy**: `ExtensionContext` -> `ExtensionToolContext` (adds `tools` + nested `execute_tool` with `<parentId>/<n>` IDs, never rejects) / `ExtensionCommandContext` (adds `wait_for_idle`, `new_session`, `fork`, `navigate_tree`, `switch_session`, `reload`) -> `ReplacedSessionContext`.
- **UI mode behavior**: `"tui"` full UI; `"rpc"` dialogs & notifications over JSONL (`custom()` returns `None`, component factories ignored); `"json"`/`"print"` no-op UI (`has_ui = false`).

### `/reload` Sequence (`interactive-mode.ts#L6714-L6762`, `agent-session.ts#L3831-L3873`)
1. Reject while `is_streaming` or `is_compacting`.
2. Capture `previous_flag_values`.
3. Emit `session_shutdown { reason: "reload" }` on old runner and call `invalidate()`.
4. Reload settings, reset API providers, re-discover and freshly instantiate extensions.
5. Preserve active tools + newly added `default_tools`; stage in `_pending_tool_names` for late-registering tools (e.g., MCP).
6. Bind new runner, restore `previous_flag_values`, flush providers/virtual models, refresh tools.
7. Emit `session_start { reason: "reload" }`, check `report_unhandled_mcp_servers()`, emit `resources_discover { reason: "reload" }`, refresh TUI keybindings/themes/autocomplete/shortcuts.

## 3. Chord Plugin World (`packages/chord/src/node/plugins/` & `src/host/`)

Distinct from coding-agent's `ExtensionAPI`:
- **Digest-based identity (`build.ts#L87-L172`, `surfaces/loaded.ts#L34-L152`)**: Each plugin facet (`server`, `worker`, `web`, `tui`) is hashed (SHA-256 of `.wasm` component bytes). Unchanged digests keep exact loaded instance identity across rollouts.
- **Synchronous multi-pass staging (`host/host.ts#L766-L1075`, `staging.ts#L103-L500`)**:
  1. Stage new/changed facets (`facet.setup()`, check duplicate contract/registry names, `accepts`, `admit`).
  2. Dry-run `record_factory` against an inert `RecordingSelf` for every live/retrying object of changed/removed plugins to collect declared `documents`, `tables`, `jobs`, `contributions`, and `requests` with zero side effects.
  3. **Closure rule**: On any build/eval/stage failure, transitively hold back any changed plugin whose recorded requests would fail to bind or would bind to a failed plugin's old code (iterated to fixed point).
  4. **Atomic commit**: Swap served types & registries in a single batch commit.
  5. **Object fates (`object.ts#L1113-L1148`, `reload-cycle.ts#L21-L97`)**: `end` (type removed), `retry` (not live), `restart` (old instance enters `begin_drain()` up to `reload_drain_timeout`; new calls gate; `ReloadCycles` breaks drain wait cycles immediately with `unavailable/reload_cycle`; successor activates once drained & ready).

## 4. WIT Boundary Semantic Traps

1. **In-place JS mutations -> explicit WIT return patches**: `before_provider_headers` (`headers`), `before_agent_start` (`systemPromptOptions`), `tool_call` (`input`), and `context` (`messages`) mutate JS objects in place. WIT return records must carry explicit `option<patch>` payloads so value-copied WASM calls preserve exact hook semantics.
2. **Live TUI component factories across WASM**: Native built-in extensions implement `pi-rs-tui::Component` directly; WASM extensions use declarative UI (`set-widget` lines, `register-markdown-transformer`) plus an optional WIT `custom-component` resource (`render(width) -> list<string>`, `handle-input(data) -> bool`) without exposing raw host object pointers.
3. **Host re-entrancy**: `wasmtime::Store` cannot be borrowed twice concurrently on the same instance; serialize or queue re-entrant host->guest callbacks per instance while allowing guest->host async calls (`ctx.ui.*`, `ctx.execute_tool`) to yield cleanly.
