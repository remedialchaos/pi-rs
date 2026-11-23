# Milestone 3.2-web: `pi-rs-p2-web`

**Status: complete.** Leptos `0.8.x` (CSR) WASM browser frontend porting `apps/p2/src/web/`
per ADR-011. All authored code is Rust (edition 2024, `#![forbid(unsafe_code)]`); upstream
`apps/` and `packages/` are untouched.

## Delivered

- **CSS assets** (`assets/`, embedded with `include_str!` in `src/assets.rs`): exact copies of
  `elanor.css`, `elanor-markdown.css`, `style.css`, `owner-notices.css`,
  `activity/activity.css`, `command-menu/command-menu.css`, `elanor-extra/elanor-extra.css`.
  `STYLESHEETS` gives the server a stable, byte-exact drop-in serving order (ADR-003).
- **Pure state/protocol logic**, native-testable and wasm32-clean:
  - `state/`: conversation cache window + MRU eviction, chat-context relative paths, composer
    state (attachment references, size validation, draft persistence), `@`/Tab file completion
    token handling and popover state machine, artifact kinds, scroll anchor binary search,
    cross-line syntax highlight tree splitting.
  - `command_menu/`: fuzzy scorer with word-start/contiguous bonuses, DOM-free highlight state
    machine, recents, route-layer locators, default key table, target priority, context types.
  - `activity/`: step/detail/thought/results projections, labels, summaries, nested call grouping,
    result placement.
  - `gestures/`: drag axis/fling/settle math, drag-size clamping, drawer swipe, sheet rubber band,
    swipe-back, picker drum physics, haptics codec.
  - `cost.rs`: cost/axis/percent formatting, totals, bar heights.
  - `chord_bridge.rs`: Chord binary frame varint/kind-flag/part codec, reconnect backoff/stability/
    staleness state machine, p2-mac `piHost` + `pi-host-theme` message codecs, Radius shell theme
    validation.
  - `markdown.rs`: `pulldown-cmark` GFM rendering to safe HTML with upstream's Elanor classes
    (raw HTML escaped, unsafe links and remote images demoted, code blocks highlighted).
- **Leptos CSR components** (`src/ui/`): `AppLayout`/`Workbench`/`AppBoundary`/`ErrorToast`,
  `Chat`/`MessageRow`/`Code` with fine-grained streaming signal updates, `ActivityRow`/
  `ActivitySheet`/`ThoughtSlot`/`Results`, `CommandMenu`/`Header`/`List`, `CostPanel`, `Composer`/
  `FileCompletionPopover`, `Terminal`/`Voice`. Upstream CSS classes and `data-*` attributes are
  preserved.
- **Entry point** (`src/wasm.rs`): `#[wasm_bindgen(start)]` / `mount_app()`, gated on
  `target_arch = "wasm32"` so native builds stay DOM-free.

## Verification

| Gate | Result | Evidence |
| --- | --- | --- |
| `cargo fmt --check` | PASS | no diff |
| `cargo clippy --all-targets -- -D warnings` | PASS | 0 warnings |
| native `cargo test` | PASS | **134/134** (126 unit + 8 integration) |
| `cargo check --target wasm32-unknown-unknown` | PASS | full Leptos CSR component graph compiles |
| mutation checks | PASS | 3/3 caught (XSS escaping, empty-query scoring, rubber-band geometry); each broken behavior restored |

Commands run from the worktree with `CARGO_TARGET_DIR=/var/tmp/pi-rs-target-oc-hy4`,
`CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_PROFILE_TEST_DEBUG=0`, `--offline -j 2`.

## Known limits

- DOM-bound behavior (pointer/touch event capture, `ResizeObserver`, Web Audio, IndexedDB,
  `matchMedia`, `piHost` postMessage) lives in the host/browser; this crate ports the **pure
  logic, geometry, codecs and state machines** plus the reactive component tree, with the host
  responsible for wiring timers/sockets/storage to the emitted actions.
- Chord binary framing ports the documented primitives (varints, kind-byte flags, part framing)
  rather than every message kind of the full upstream codec; the shared wire types remain in
  `pi-rs-chord` for end-to-end use.
- No live WKWebView/browser runtime test (no macOS/desktop browser in this environment);
  wasm32 compilation is the enforced gate.
