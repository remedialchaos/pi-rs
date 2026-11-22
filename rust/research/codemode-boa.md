# Primary-Source Research: `pi-rs-codemode` (`boa_engine` vs `quickjs-wasi`)

Covers Milestone 1.4 (`packages/codemode` -> `pi-rs-codemode` + `packages/coding-agent/src/extensions/codemode/`). Every claim cites upstream source or `docs.rs/boa_engine`.

## 1. Crate Choice: `boa_engine` (`v0.21+` / `v0.22`, `docs.rs/boa_engine`)

Per ADR-002, `pi-rs-codemode` replaces `quickjs-wasi` (`3.6.2`) with pure-Rust **`boa_engine`**:
- **Zero C / WASI toolchain**: compiles anywhere `rustc` compiles with zero ambient FS, network, env, stdout/stderr, or timers.
- **Async / Promise bridge (`boa_engine::job`)**:
  - Implement a custom `JobExecutor` (`enqueue_job(self: Rc<Self>, job: Job, context: &mut Context)`) handling `Job::Promise` and `Job::NativeAsync` (`docs.rs/boa_engine/latest/boa_engine/job/`).
  - Do **not** use `SimpleJobExecutor` (blocks synchronously on futures). Use a custom executor that drains promise microtasks in bounded batches (`JOBS_PER_POLL = 50`, matching `vm.ts#L154,L247`), checks cancellation/interrupt flags between batches, and drives pending host tool/model calls concurrently on Tokio.
- **Execution limits (`Context::runtime_limits_mut()`, `docs.rs/boa_engine`)**:
  - `set_loop_iteration_limit(n)` catches `while (true) {}` with an **uncatchable** `RuntimeLimitError::LoopIteration` bubbling out to Rust.
  - `set_recursion_limit(n)` catches infinite recursion before Rust stack overflow (`MAX_STACK_SIZE = 512 KiB` worker / `256 KiB` inline in `vm.ts#L55`, `inline.ts#L42`).
  - **Known gap — heap memory cap (`256 MiB` default in coding-agent, `execute.ts#L56`; `32 MiB` remote, `cloudflare.ts#L32`)**: `boa_engine` does **not** have a built-in byte-exact per-realm heap limit like QuickJS's `JS_SetMemoryLimit`.
    - **Mitigation design**: Enforce string/array/store/output caps in native bindings (`MAX_OUTPUT_CHARS = 16 MiB`, `MAX_OUTPUT_ITEMS = 100_000`, `MAX_STORE_VALUE_CHARS = 256 KiB`, `MAX_STORE_TOTAL_CHARS = 1 MiB`), enforce `RuntimeLimits` (`loop_iteration_limit`, `recursion_limit`, `stack_size_limit`), AND run untrusted scripts on a dedicated bounded worker thread/task (or subprocess with `RLIMIT_AS` / tracked allocator if hard OOM containment is needed for adversarial scripts). Document this explicitly in `pi-rs-codemode`.
- **Host hooks (`boa_engine::context::HostHooks`)**: Supports custom `make_job_callback`, `ensure_can_compile_strings`, and clock/PRNG control for deterministic mode (`CodemodeSandboxBase.start()`).

## 2. Exact Upstream Sandbox Contract to Preserve

### Source Parsing (`packages/codemode/src/source.ts#L11-L125`)
- Prefix `// @options:` is valid **only on line 1** (`trimStart().startsWith(...)`); anywhere else throws `CodemodeSourceError`.
- Allowed JSON keys only: `max_output_tokens` (non-negative safe int, including `0`) and `timeout_ms` (`1..=2_147_483_647`).
- Line 1 is replaced with `""` (preserving `\n`) so stack line numbers stay 1:1 aligned; whitespace-only remaining code throws `CodemodeSourceError`.
- Evaluated wrapped as `` `(async (tools, console) => {${code}\n})` `` with filename `"codemode.js"` (`vm.ts#L325-L334`).

### Built-in Globals (`prelude-source.ts#L58-L458`, `execution.ts#L51-L61`)
Reserved names: `tools`, `ALL_TOOLS`, `console`, `text`, `image`, `exit`, `globalThis`, `store`, `load`.
All `globalThis` configurable data properties are locked `{ writable: false, configurable: false }` and reachable intrinsics frozen with the override-mistake accessor workaround (`prelude-source.ts#L54-L139`).

| Global | Exact Contract |
| --- | --- |
| `tools` | Null-prototype frozen object wrapped in `guard` `Proxy`. Bound at both `tools[jsName]` (`toCodemodeIdentifier`: `[A-Za-z_$][A-Za-z0-9_$]*`, invalid -> `_`, empty -> `"_"`) and `tools[rawName]` (first tool wins on collision in `execute()`; `start()` requires unique names). Missing prop read throws `TypeError` with up to 5 fuzzy suggestions (`"then"`, `"toJSON"`, `Object.prototype` keys, and Symbols pass through). |
| `ALL_TOOLS` | Frozen array of frozen `{ name: jsName, description }`, deduped by first `jsName`. |
| `text(v)` | `undefined`/`null`/primitives -> `String(v)`; objects/functions -> `JSON.stringify(v)` (fallback `String(v)` if `stringify` returns `undefined`; throws `TypeError` if `stringify` throws). |
| `image(v)` | Accepts string `data:;base64,...`, `{ image_url }`, or `{ type: "image", data, mimeType? }`. Rejects `http:`/`https:`. Strips `\s+`, validates base64, **ignores declared MIME**, sniffs MIME strictly from base64 magic prefix: PNG (`^iVBORw0KGg`), JPEG non-LS (`^/9j/(?!9)`), GIF (`^R0lGOD[dl]h`), WebP (`^UklG.{8}RUJQ`). |
| `console.{log,info,warn,error,debug}` | Joins `args.map(format)` with `" "`. `format` never throws (strings verbatim, `Error` -> `errorText`, numbers -> `String(v)`, else `JSON.stringify` catching errors -> `String(v)`). Emits `{ type: "text", text, console: true }`. |
| `store(k, v)` / `load(k)` | Sync `Map<String, String>` in-VM. Non-string key -> `TypeError`; `undefined` deletes key. Limits: single JSON value `<= 262_144` chars, total `sum(key.len + json.len) <= 1_048_576` chars (`RangeError`). `storeWrites: { set, delete }` committed **only when `result.ok == true`**. |
| `exit()` | Serializes writes, invokes `done(true, undefined, writesJson)`, then throws frozen `EXIT` sentinel. |
| Host globals & namespaces | `<id>` -> `globalThis[id]`; `<ns>.<member>` -> frozen null-proto `guard`-proxied object at `globalThis[ns]`. Excluded from `result.calls`. |
| Coding-agent extras | `searchTools(query, { limit?, namespace? })` (BM25, default limit 8), `describeTool(name)`, `describeNamespace(name)`, `models.{getModelsOfType, getAvailableOfType, getModelOfType, classify, generateImages}` (max 4 concurrent model calls, `headers` stripped). |

### Limits & Error / Promise Semantics
- **Output limits**: In-VM hard cap `MAX_OUTPUT_CHARS = 16_777_216` and `MAX_OUTPUT_ITEMS = 100_000` (calls `done(false, ...)` *before* throwing `RangeError` so catching cannot recover). Coding-agent token truncation: `DEFAULT_MAX_OUTPUT_TOKENS = 10_000` (`4` chars/token = `40_000` chars), keeping half head + half tail and spilling full output to a temp `.txt` file.
- **Worker concurrency (coding-agent)**: `DEFAULT_MAX_CODEMODE_WORKERS = 2` (`PI_CODEMODE_MAX_WORKERS`), wait queue capped at `60_000 ms`.
- **Timeouts & cancellation**: `DEFAULT_TIMEOUT_MS = 300_000` (`Infinity` disables); first terminal event wins in `Execution.finish()`; aborts all in-flight tool calls and preserves partial `output` + `calls` on failure.
- **Stalled promise detection (`vm.ts#L242-L250`, `prelude-source.ts#L511-L522`)**: After draining all runnable promise jobs, if `!finished && pending_host_calls.is_empty()`, immediately fail with `"Error: The script is waiting on a promise that can never settle: no tool call is pending, and timers do not exist here."`.
- **Error kinds**: `"script"` (parse, uncaught throw/rejection, stalled, output limit; strips `codemode-prelude.js` stack frames), `"timeout"`, `"aborted"`, `"sandbox"`.

## 3. Semantic Traps

1. **Spread global `undefined` -> `null`**: `caller("global", name, true)` serializes args as a JSON array (`[arg0, arg1]`), turning `undefined` into `null`; non-spread `tools.foo()` keeps omitted/undefined `args[0]` as `undefined` (`prelude-source.ts#L204`).
2. **Nested tool `isError` with `outputSchema` (`coding-agent/src/extensions/codemode/execute.ts#L480-L486`)**: When `tool.outputSchema` is set and `structuredContent !== undefined`, `tools.<name>()` **resolves** to `structuredContent` even if `outcome.isError === true` (e.g., `bash` non-zero exit codes, MCP `CallToolResult`).
3. **`text()` vs `console.*` on `NaN`/`Infinity`/`-0` and circular objects**: Both format numbers via `String(v)` (not `JSON.stringify` -> `"null"`). `text(circular)` throws `TypeError`, whereas `console.log(circular)` falls back to `String(v)`. `text(undefined)` emits `"undefined"`, whereas `return undefined` emits nothing.
4. **Catching `exit()` or output-limit `RangeError`**: Because `done(...)` is recorded *before* throwing, catching inside `try/catch` blocks further `done`/`output` writes while trailing sync code drains (`prelude-source.ts#L153-L154,L348-L355,L433-L443`).
5. **Deterministic mode (`CodemodeSandboxBase.start()`)**: Pins UTC (`timezoneOffset = 0`), removes `WeakRef`/`FinalizationRegistry`, seeds PRNG deterministically (mulberry32 over `seed ^ 0x9e3779b9`), and drives monotonic clock timestamps per host delivery (`vm.ts#L84-L125,L178`). Verify Boa's `Date` / `Math.random` override hooks when implementing `start()`.
