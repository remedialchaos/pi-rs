# Milestone 3.2-web: `pi-rs-p2-web`

**Scope: browser UI and Chord session/chat host.** Leptos `0.8.x` (CSR) WASM browser frontend porting `apps/p2/src/web/`
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

## Earlier UI checkpoint

| Gate | Result | Evidence |
| --- | --- | --- |
| `cargo fmt --check` | PASS | no diff |
| `cargo clippy --all-targets -- -D warnings` | PASS | 0 warnings |
| native `cargo test` | PASS | **134/134** (126 unit + 8 integration) |
| `cargo check --target wasm32-unknown-unknown` | PASS | full Leptos CSR component graph compiles |
| mutation checks | PASS | 3/3 caught (XSS escaping, empty-query scoring, rubber-band geometry); each broken behavior restored |

Commands run from the worktree with `CARGO_TARGET_DIR=/var/tmp/pi-rs-target-oc-hy4`,
`CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_PROFILE_TEST_DEBUG=0`, `--offline -j 2`.

## Browser host and assets

The WASM start function mounts into `#root`. A locally served page reads the current
base bundle from `globalThis.chordPage.base` and opens `/chord?web=<base>`. A
Radius page reads `data-pi-bootstrap` from the root and sets `client=cloud` on its
Chord URL. The browser supplies the session cookie during the WebSocket upgrade.

The host exchanges binary Chord hello frames, holds Access v1 and Sessions v7 on
the server, and holds Chat v6 at the selected session worker. Owners can create
a session using the catalog's default directory and environment. The composer
calls the participant's `send` with the text, request ID, and `whenBusy: "steer"`.
The server's conversation view drives the transcript. User rows read the person's
entry data, assistant rows read model text, and live partials read
`docs["pi.live"].generation.message`. Snapshot, delta, acknowledgment, and
ping/pong handling run in Rust. A reconnect re-holds the selected conversation.

### Build the browser assets

Run from the worktree root with the wasm32 target, the wasm-bindgen CLI matching
Cargo.lock (0.2.130 here), and the upstream npm dependencies available.

```sh
export TMPDIR=/var/tmp/pi-rs-tmp-p2-browser-host
export CARGO_TARGET_DIR=/var/tmp/pi-rs-target-p2-browser-host
export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
assets="$TMPDIR/assets"
mkdir -p "$assets"
cargo build --manifest-path rust/crates/pi-rs-p2-web/Cargo.toml \
  --target wasm32-unknown-unknown --offline -j 1
wasm-bindgen --target web --out-name app --out-dir "$assets" \
  "$CARGO_TARGET_DIR/wasm32-unknown-unknown/debug/pi_rs_p2_web.wasm"
npm exec --offline -- esbuild rust/crates/pi-rs-p2-web/assets/browser.css \
  --bundle --loader:.woff2=file --outfile="$assets/p2.css"
```

Serve `app.js`, `app_bg.wasm` (MIME `application/wasm`), `p2.css`, and the five
generated WOFF2 files from one asset directory. Link the stylesheet and provide
`<div id="root"></div>`. In the page's permitted module entry, import the default
export from `./app.js` and call `await init()` once. Use the current Chord web
base on a local page, or supply a JSON object with a `chordUrl` string in the
root's `data-pi-bootstrap` attribute. A route serving the HTML, a valid owner or
guest session, and the upstream-compatible WebSocket endpoint are host dependencies.

### Parity boundary

This slice covers the Access, Sessions and Chat contracts, text submissions and text
transcripts. Browser plugin loading/hot reload, telemetry export, shell and native
theme bridges, attachment upload, tool cards, and offline transcript persistence
remain outside the implemented host flow. WebKit and guest share expiration have
no end-to-end result here. The HTTP front-door oracle (6/6, coordinator-reported)
and replay belong to the coordinator. This browser entry has no standalone
rust-anything leaf recording, diff count or leaf gate.

### Browser-host verification

| Gate | Result | Evidence |
| --- | --- | --- |
| Chromium with compiled WASM and upstream p2 over loopback | PASS | Signed in as the disposable fixture's local owner. Clicked New session, sent `browser-loopback`, and observed distinct committed user and assistant rows containing exactly the input and the faux provider's `Echo: browser-loopback` response. Reloaded the page and observed the persisted assistant row from a fresh connection. |
| Assets | PASS | wasm-bindgen produced a 6,073,220-byte debug WASM module and a 51,469-byte JS loader. esbuild bundled the CSS and five fonts. |
| Format and Clippy | PASS | Crate formatting, native all-target Clippy, and wasm32 library Clippy with warnings denied. |
| Native tests | PASS | Before 134/134, 4m10.168s wall time including dependency compilation. After 135/135, 21.603s with dependencies cached. Unit test execution alone was 0.03s before and 0.01s after. Both integration runs reported 0.00s. |
| Mutation | PASS | One test added, zero removed. Keeping the UTF-16 prefix instead of removing it made the cross-frame path test return `draft ` instead of `😀 live`. Restoring the operation passed the suite. |

The browser check used the upstream fixture's real server and session worker with
a fake model and an isolated catalog. Chromium loaded the generated assets through
DevTools on the fixture's HTTP origin, with a Cloud Brain bootstrap and CSP bypass
for the injected test page. This verifies the WASM and authenticated binary WebSocket
flow. Production HTML routing, CSP, and the server's asset packaging require separate
integration verification. Cargo regenerated the local lockfile, including an unrelated
offline resolution change to jiff-tzdb from 0.1.10 to 0.1.8.
