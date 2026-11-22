# Primary-Source Research: Rust-to-WASM Web UI (`leptos` vs `dioxus`)

Covers Phase 3 web frontends: `packages/chord/src/{web,react}/`, `apps/p3` web surface (Milestone 3.1), and `apps/p2/src/web/` + `apps/p2/plugins/*/web.tsx` (Milestone 3.2 — 96 UI units, ~15k+ LOC TSX/CSS).

## 1. Upstream Web Architecture Inventory (`apps/p2/package.json`, `apps/p2/src/web/`, `packages/chord/src/react/`)

- **Rendering mode**: Pure Client-Side Rendering (CSR) served as static assets from `p2`'s embedded HTTP server (`apps/p2/src/hosts/web/`), talking to the backend exclusively over **Chord WebSocket binary links** (`packages/chord/src/web/`) and REST endpoints (`/owner/local/*`, `/api/*`, `/artifacts/*`, `/files/*`). No SSR or hydration is used.
- **State model (`packages/chord/src/react/`)**: Fine-grained reactive subscriptions to Chord `Readable` / `Document` / `LiveQuery` objects (`useReadable`, `useObject`) updated incrementally via Chord delta patches (`packages/chord/src/delta/`).
- **UI features in `apps/p2/src/web/`**:
  - Streaming chat transcript with live token appends, collapsible tool activity rows, thought slots, syntax-highlighted code blocks, Markdown + Mermaid diagrams, cost panel.
  - Multi-pane workbench (`Workbench.tsx`, drag-resize handles, sheet/drawer swipe gestures, command menu with fuzzy highlight, file autocomplete).
  - Embedded interactive terminal (`Terminal.tsx`), voice audio capture/playback WebSocket bridge, macOS host bridge (`window.webkit.messageHandlers.piHost` / `pi-host-theme` custom events), CSS design tokens (`@earendil-works/design-tokens` / `elanor.css`).
  - Dynamic plugin web facets (`plugins/*/web.tsx`): in 100% Rust (ADR-002), built-in `p2` plugins compile statically into the WASM UI bundle via Rust view registry traits; external dynamic plugin views use declarative JSON/WIT UI specs rendered by the host WASM shell.

## 2. Framework Evaluation: `leptos` (`0.8.x`) vs `dioxus` (`0.7.x`)

| Criterion | `leptos` (`0.8.x`) | `dioxus` (`0.7.x`) | Fit for `pi-rs` |
| --- | --- | --- | --- |
| Reactivity & streaming chat perf | Fine-grained signals update exact text/DOM nodes with zero VDOM diffing | Optimized VDOM + generational arenas + `use_signal` | **`leptos` wins**: streaming LLM tokens and high-frequency Chord delta updates mutate leaf DOM nodes directly without re-diffing long transcripts |
| Chord `Readable` / `LiveQuery` fit | Maps 1:1 onto fine-grained reactive `ReadSignal` / `RwSignal` / `Memo` | Works via `use_signal`, but still triggers component VDOM rerenders | **`leptos` wins** |
| WASM bundle size & startup | Smaller CSR WASM payload (no VDOM reconciler) | Slightly larger runtime overhead | **`leptos` wins** |
| Direct DOM / WebKit interop (`web-sys`) | First-class direct DOM node refs (`NodeRef`), custom events (`pi-host-theme`), `ResizeObserver`, pointer drag gestures, `AudioWorklet`, Canvas/Terminal | Supported via `web-sys` / `eval`, slightly more ceremony around raw DOM elements | **`leptos` wins** |
| Cross-platform desktop sharing | Web-focused (pairs cleanly with our native `p2-mac` `WKWebView` shell & native `pi-rs-tui`) | Unified Web + Desktop (`dioxus-desktop`) | **Neutral**: `pi-rs` already has a dedicated native TUI (`pi-rs-tui`) and native macOS `WKWebView` shell (`p2-mac`) that loads the web UI directly |
| React TSX porting ergonomics | `view! { ... }` HTML-like syntax; small mental shift from React hooks to signals (avoiding stale closures) | `rsx! { ... }` closest to React hooks | Slight edge to `dioxus`, minor given 100% Rust rewrite |

## 3. Decision Recommendation (for ADR before Phase 3.2)

- **Recommended primary choice**: **`leptos` (`0.8.x`, `csr` feature)** compiled to `wasm32-unknown-unknown` with `wasm-bindgen` (both toolchain targets already verified green in `rust-anything doctor`) and `gloo-net` / `web-sys` for Chord WebSocket binary frames, Web Audio, and `pi-mac` host-bridge events.
- **Why `leptos`**:
  1. `p2`'s hottest UI paths are streaming assistant messages, live terminal/activity feeds, and incremental Chord delta updates — fine-grained DOM signals eliminate transcript re-diffing lag.
  2. `p2-mac` loads the web bundle inside `WKWebView` and relies heavily on direct DOM geometry (`data-shell-corner`, `--shell-inset-start`, `window.webkit.messageHandlers.piHost`, CSS view transitions) where Leptos's real-DOM `NodeRef` model has zero VDOM impedance mismatch.
  3. Reusing the exact existing CSS files (`elanor.css`, `elanor-markdown.css`, `activity.css`, `command-menu.css`, plugin `style.css`) works verbatim with `leptos`'s standard HTML/class syntax.
- **Shared crate strategy**: Keep `pi-rs-chord`'s `delta`, `link` binary codec, `contracts`, `surfaces`, and `web` client protocol strictly `wasm32`-clean (no `tokio::net` / `rusqlite` when `default-features = false`) so the browser WASM bundle links the exact same Rust wire/delta code as the `p2` server!
