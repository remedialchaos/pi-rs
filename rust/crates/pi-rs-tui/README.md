# `pi-rs-tui`

Terminal user interface components with differential rendering, ported from `@earendil-works/pi-tui`.

The crate follows the upstream module boundaries. It provides regular and alternate-screen
renderers, input and editor components, layout and scrolling, terminal image protocols, and
terminal key parsing.

## Layout

| Upstream file | Rust module |
| --- | --- |
| `src/utils.ts` | `utils` |
| `src/oklab.ts`, `src/colors.ts`, `src/terminal-colors.ts` | `oklab`, `colors`, `terminal_colors` |
| `src/fuzzy.ts` | `fuzzy` |
| `src/keys.ts`, `src/keybindings.ts` | `keys`, `keybindings` |
| `src/kill-ring.ts`, `src/undo-stack.ts`, `src/stdin-buffer.ts`, `src/wheel-scroll.ts` | `kill_ring`, `undo_stack`, `stdin_buffer`, `wheel_scroll` |
| `src/word-navigation.ts`, `src/latex.ts`, `src/program-status.ts` | `word_navigation`, `latex`, `program_status` |
| `src/native-platform.ts` | `native_platform` |
| `src/terminal.ts`, `src/terminal-image.ts` | `terminal`, `terminal_image` |
| `src/tui.ts`, `src/layout-node.ts`, `src/layout.ts` | `tui`, `layout_node`, `layout` |
| `src/tui-main-screen.ts`, `src/tui-alt-screen.ts`, `src/alt-screen-search.ts` | `tui_main_screen`, `tui_alt_screen`, `alt_screen_search` |
| `src/autocomplete.ts`, `src/editor-component.ts` | `autocomplete`, `editor_component` |
| `src/components/*.ts` | `components::*` |

`PORTING.md` next to this file freezes the interfaces the modules share.

## Dependencies

| Upstream | Rust |
| --- | --- |
| Unicode width, emoji and segmentation | `unicode-width`, `unicode-segmentation`, `emojis`, ICU4X |
| `marked` + `marked-cjk-friendly` | `pulldown-cmark` with CJK emphasis, math and link extensions |
| `chalk` | ANSI escape sequences and caller-supplied text styles |
| `@xterm/headless` | `vt100` for Rust rendering tests; upstream xterm for reference captures |
| Process terminal | `rustix` on Unix; `crossterm` and `windows-sys` on Windows |
| Native clipboard helpers | `x11rb` on Linux; clipboard reads unavailable on macOS and Windows |

## Verification

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Fixtures under `tests/fixtures/` are outputs captured from the unchanged TypeScript reference:

| Rust test | Upstream behavior replayed |
| --- | --- |
| `editor_oracle` | 154 multiline editor and 47 input traces: text, cursor and rendered rows |
| `markdown_oracle` | 229 frames: plain rendered rows after stripping terminal controls and trailing padding |
| `utils_oracle` | 181 cases: Unicode cell widths, grapheme boundaries, normalization, truncation, wrap and slice bytes |
| `tui_main_screen` | 18 viewport frames: differential updates, shrink and regrowth, and hardware cursor positions |

Rust integration tests also exercise overlay boundaries, alternate-screen wheel and search,
selection, terminal images, and Linux PTY shutdown and job-control restoration.

| Milestone 1.5 gate | Evidence |
| --- | --- |
| Native crate | Formatting and Clippy clean; 96/96 Rust tests |
| Fuzzy | 354/354 leaf replay cases |
| Terminal colors | 615/615 leaf replay cases |
| Oklab | 1,129/1,129 leaf replay cases |
| Colors | 2,960/2,960 leaf replay cases |
| Keys | 2,724/2,724 leaf replay cases |

The leaf packages under `rust/pi-rs-tui-*` exercise the JavaScript bindings with
`rust-anything gate`. The bridge-inclusive benchmark was slower than the TypeScript reference
overall for each leaf; it does not measure native application rendering.
The recorded cases do not establish exact parity for every input, theme, or terminal.
Native runtime verification was on Linux x86-64.
