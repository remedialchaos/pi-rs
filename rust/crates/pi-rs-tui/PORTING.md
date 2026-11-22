# Porting contracts (`pi-rs-tui`)

Every module of this crate is ported from `packages/tui/src/`. This file freezes the shared interfaces: a module
that another module uses must keep the signatures below.

## Rules

- Rust 2024, stable. No `unsafe` unless a platform module needs it (with a `// SAFETY:` comment). No
  `#[allow(...)]`; use `#[expect(..., reason = "...")]`. No `unwrap()`, `expect()` or `panic!` on a fallible path.
- A function that can fail returns `Result<T, E>`; `E` is a `thiserror`-style enum or `String` when the upstream
  throws a plain `Error`. The upstream's `throw new Error(message)` becomes `Err(...)` with the same message.
- A TypeScript `string` is a Rust `String`; a `number` is `f64` unless it counts things (then `usize`).
- A TypeScript `readonly T[]` is `&[T]`; a returned array is `Vec<T>`.
- A TypeScript object with optional fields is a Rust struct with `Option` fields, built with a `Default` and
  `..Default::default()`.
- Port the upstream tests of the module to `tests/<module>.rs` (integration test) or to `#[cfg(test)]` unit tests
  inside the module. A test that cannot run on Linux (a Windows or macOS only path) is skipped with a comment
  saying why.
- The upstream test files are under `packages/tui/test/`. The upstream test runner is
  `node --test test/*.test.ts`; the helpers `packages/tui/test/virtual-terminal.ts` and `test-themes.ts` are shared.

## Module map

| Upstream file | Rust module | Owner |
| --- | --- | --- |
| `src/utils.ts` | `utils` | text utilities |
| `src/oklab.ts` | `oklab` | colors |
| `src/colors.ts` | `colors` | colors |
| `src/terminal-colors.ts` | `terminal_colors` | colors |
| `src/fuzzy.ts` | `fuzzy` | colors (leaf) |
| `src/keys.ts` | `keys` | input |
| `src/keybindings.ts` | `keybindings` | input |
| `src/word-navigation.ts` | `word_navigation` | text utilities |
| `src/kill-ring.ts` | `kill_ring` | input |
| `src/undo-stack.ts` | `undo_stack` | input |
| `src/stdin-buffer.ts` | `stdin_buffer` | input |
| `src/wheel-scroll.ts` | `wheel_scroll` | input |
| `src/program-status.ts` | `program_status` | terminal |
| `src/latex.ts` | `latex` | text utilities |
| `src/native-platform.ts` | `native_platform` | platform |
| `src/terminal.ts` | `terminal` | terminal |
| `src/terminal-image.ts` | `terminal_image` | terminal |
| `src/tui.ts` | `tui` | core |
| `src/layout-node.ts` | `layout_node` | core |
| `src/layout.ts` | `layout` | core |
| `src/tui-main-screen.ts` | `tui_main_screen` | core |
| `src/tui-alt-screen.ts` | `tui_alt_screen` | core |
| `src/alt-screen-search.rs` | `alt_screen_search` | core |
| `src/editor-component.ts` | `editor_component` | components |
| `src/autocomplete.ts` | `autocomplete` | components |
| `src/components/*.ts` | `components::*` | components |

## Frozen: `tui` (the component interface)

```rust
/// The kind of a mouse event.
pub enum TuiMouseEventType { Press, Release, Move, Drag, Click, Wheel }
/// The button of a mouse event.
pub enum TuiMouseButton { Left, Middle, Right, None }

/// A mouse event, with coordinates local to the receiving component.
pub struct TuiMouseEvent {
    pub event_type: TuiMouseEventType,
    pub button: TuiMouseButton,
    /// Coordinates local to the receiving component.
    pub x: usize,
    pub y: usize,
    /// Absolute terminal coordinates.
    pub screen_x: usize,
    pub screen_y: usize,
    /// The bounds of the receiving component.
    pub width: usize,
    pub height: usize,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
    /// Logical lines; negative values scroll up.
    pub wheel_delta: Option<i32>,
    /// Consecutive click count when the event type is `Click`.
    pub click_count: Option<u32>,
}

/// What a component did with a mouse event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TuiMouseEventResult {
    /// Stop propagation and suppress the renderer-level fallback.
    pub handled: bool,
    /// Route the later drag and release events to this component. Implies `handled`.
    pub capture: bool,
    /// Give the keyboard focus to this component. Implies `handled`.
    pub focus: bool,
    /// Whether to render again. Move and release default to false, the others to true.
    pub render: Option<bool>,
}

/// A component that can receive the keyboard focus and draw a hardware cursor.
pub trait Focusable {
    /// Whether the component has the focus.
    fn is_focused(&self) -> bool;
    /// Sets the focus.
    fn set_focused(&mut self, focused: bool);
}

/// Every component of the TUI.
pub trait Component {
    /// The lines to draw for a viewport of the given width.
    fn render(&self, width: usize) -> Vec<String>;

    /// Handles the keyboard input while the component has the focus.
    fn handle_input(&mut self, _data: &str) {}

    /// Handles a normalized mouse event.
    fn handle_mouse(&mut self, _event: &TuiMouseEvent) -> Option<TuiMouseEventResult> {
        None
    }

    /// Whether the component receives the key release events (Kitty protocol).
    fn wants_key_release(&self) -> bool {
        false
    }

    /// Whether the fullscreen viewport keys go to the component instead of the transcript.
    fn captures_viewport_input(&self) -> bool {
        false
    }

    /// Invalidates the cached rendering state.
    fn invalidate(&mut self) {}

    /// The component as a focusable one, when it is one.
    fn as_focusable(&mut self) -> Option<&mut dyn Focusable> {
        None
    }
}
```

The upstream's `focused` field on a component becomes `Focusable::set_focused`; a component that can be focused
implements `Focusable` and returns itself from `as_focusable`.

## Frozen: `utils`

```rust
/// The width of a string in terminal cells.
pub fn visible_width(text: &str) -> usize;

/// The text without the terminal escape sequences.
pub fn strip_terminal_sequences(text: &str) -> String;

/// The text without the control characters a terminal cannot show.
pub fn replace_control_characters(text: &str) -> String;

/// The text wrapped to the given width, keeping the escape sequences.
pub fn wrap_text_with_ansi(text: &str, width: usize) -> Vec<String>;

/// The text cut to the given width, keeping the escape sequences.
pub fn truncate_to_width(text: &str, width: usize, keep_ansi: bool) -> String;

/// The part of a line that covers the given columns.
pub fn slice_by_column(line: &str, start_col: usize, length: usize, strict: bool) -> String;
```

`utils.rs` also holds the helpers the other modules use: `normalize_terminal_output`, `extract_ansi_code`,
`get_active_background_ansi`, `apply_background_to_line`, `extract_segments`, `get_grapheme_cell_range`,
`get_osc8_link_at_column`, `is_whitespace_char`, `is_punctuation_char`, the grapheme and word segmenters, and the
`AnsiCodeTracker`.

## Frozen: `colors`

```rust
/// A color: a palette index, an RGB value or an OKLCH value.
pub enum Color { Indexed(u8), Rgb(RgbColorValue), Oklch(OklchColorValue) }
/// An RGB color.
pub struct RgbColorValue { pub r: f64, pub g: f64, pub b: f64 }
/// An OKLCH color.
pub struct OklchColorValue { pub l: f64, pub c: f64, pub h: f64 }
/// OKHSL channels.
pub struct OkhslChannels { pub h: f64, pub s: f64, pub l: f64 }
/// The attributes of a text.
pub struct TextAttributes { pub bold: bool, pub dim: bool, pub italic: bool, pub underline: bool, pub inverse: bool, pub strikethrough: bool }
/// The style of a text: attributes and colors.
pub struct TextStyle { pub attributes: TextAttributes, pub fg: Option<Color>, pub bg: Option<Color> }
```

The upstream's `foregroundAnsi`, `backgroundAnsi`, `styleText`, `parseColor`, `colorToHex`, `colorToRgb`,
`colorToOklch`, `colorToOkhsl`, `mixColors`, `rgbColor`, `oklchColor`, `okhslColor` and `indexedColor` keep their
names in snake case. A value that is not finite is an error, as the upstream throws.

## Frozen: `keys`

```rust
/// The kind of a key event.
pub enum KeyEventType { Press, Repeat, Release }
/// A key.
pub enum Key { ... }
/// The name of a key, as the keybindings use it.
pub type KeyId = String;
```

`parse_key(data: &str) -> Option<Key>`, `matches_key(key: &Key, id: &str) -> bool`,
`is_key_release(key: &Key) -> bool`, `is_key_repeat(key: &Key) -> bool`,
`is_kitty_protocol_active() -> bool`, `set_kitty_protocol_active(active: bool)` and
`decode_kitty_printable(data: &str) -> Option<String>` keep their names in snake case. The upstream's global
`_kittyProtocolActive` and `_lastEventType` become a `static` behind a `Mutex` (or an `AtomicBool`), not a
mutable global.

## Terminal conventions

- The terminal writes bytes to stdout; the upstream's `process.stdout.write` becomes a `Write`.
- A terminal that needs the raw mode, the alternate screen or the keyboard protocol is behind a trait
  (`Terminal`), so the tests can use a virtual terminal (the upstream's `test/virtual-terminal.ts`).
- The escape sequences are written exactly as the upstream writes them, byte for byte.

## Frozen: `terminal` and `terminal_image`

The upstream's `Terminal` is an interface over the process' terminal. In Rust it is a trait, so the tests can use a
virtual terminal (the upstream's `test/virtual-terminal.ts`):

```rust
/// A terminal the TUI draws on.
pub trait Terminal: Send {
    /// The width of the terminal in columns.
    fn width(&self) -> usize;
    /// The height of the terminal in rows.
    fn height(&self) -> usize;
    /// Writes bytes to the terminal.
    fn write(&mut self, data: &str);
    /// The bytes the terminal has read since the last call.
    fn read_available(&mut self) -> Vec<u8>;
    /// Whether the terminal is a TTY that can enter the raw mode.
    fn is_tty(&self) -> bool;
}
```

`terminal_image.rs` keeps the upstream's module-level state (the capabilities cache, the cell dimensions and the
Kitty image metadata cache) in statics behind a `Mutex`, and exposes the upstream's functions with the same
names in snake case: `get_cell_dimensions`, `set_cell_dimensions`, `detect_capabilities`, `get_capabilities`,
`reset_capabilities_cache`, `set_capability_overrides`, `set_capabilities`, `is_image_line`,
`get_image_line_extent`, `strip_image_sequences`, `allocate_image_id`, `encode_kitty`, `delete_kitty_image`,
`delete_all_kitty_images`, `encode_iterm2`, `get_max_kitty_image_rows`, `calculate_image_cell_size`,
`calculate_image_rows`, `get_png_dimensions`, `get_jpeg_dimensions`, `get_gif_dimensions`, `get_webp_dimensions`,
`get_image_dimensions`, `render_image`, `hyperlink`, `image_fallback`. The types `ImageProtocol`,
`TerminalCapabilities`, `CellDimensions`, `ImageDimensions`, `ImageRenderOptions` keep their fields.

The escape sequences the Kitty and iTerm2 protocols use must be written byte for byte as the upstream writes
them.

## Frozen: `layout_node`

```rust
/// The viewport a layout node is laid out in.
pub struct LayoutViewport { pub width: usize, pub height: usize }
/// One entry of a stack layout node.
pub struct StackLayoutEntry { ... }
/// A stack layout node.
pub enum LayoutNode { Stack(StackLayoutNode), Scroll(ScrollLayoutNode) }
```

A component that takes part in the layout implements a `layout_node(&self) -> Option<LayoutNode>` method; the
upstream's `LAYOUT_NODE` symbol becomes that method on the `Component` trait:

```rust
pub trait Component {
    // ... as above ...
    /// The layout node of the component, when it takes part in the layout.
    fn layout_node(&self) -> Option<LayoutNode> { None }
}
```

## Components

A component is a struct that implements `Component`. The upstream's optional theme and options become Rust
structs with `Default`; a theme is a plain data struct (the upstream has no shared theme type: each component has
its own).

The upstream's `Text` component renders a string with a style; `TruncatedText` truncates it to the width;
`Input` and `Editor` are text editors with a cursor; `SelectList` and `SettingsList` are lists; `ScrollView`
scrolls; `VStack`/`HStack` lay children out; `Box` draws a frame; `Spacer` takes space; `Loader` and
`CancellableLoader` animate; `MouseRegion` reacts to the mouse; `Image` draws an inline image; `Markdown`
renders Markdown; `AltScreenFlash` flashes the alternate screen.

## The virtual terminal (tests)

The upstream's `packages/tui/test/virtual-terminal.ts` is the harness the rendering tests use: a terminal that
records what was written and can feed input back. Port it to `rust/crates/pi-rs-tui/tests/support/virtual_terminal.rs`
(a module the test files include with `mod support;`) or to a `tests/support/mod.rs`. One worker owns it; if
several need it, the first to write it owns the file and the others reuse it (ask the coordinator if it is
missing).
