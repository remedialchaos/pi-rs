# Primary-Source Research: `pi-rs-tui` Dependencies & Platform Layer

Covers Milestone 1.5 (`packages/tui` -> `pi-rs-tui`). Every claim cites upstream source (`packages/tui/`) or `docs.rs`.

## 1. External Dependencies & Rust Replacements

### `get-east-asian-width` (`1.6.0`) -> `unicode-width` (`0.2.x`, `docs.rs/unicode-width`) + Custom Terminal Width Table
- **Upstream usage**: Single import `eastAsianWidth(codePoint)` in `packages/tui/src/utils.ts#L1`, called with no options arg in `clusterWidth()` on the base code point (`#L294`) and trailing visible code points (`#L312`), returning `1` or `2`.
- **Rust mapping**: `unicode-width` (`0.2.x`, `UnicodeWidthChar::width` — **not** `width_cjk`, since upstream calls `eastAsianWidth(cp)` without `{ ambiguousAsWide: true }`, treating UAX #11 Ambiguous as width `1`), paired with `unicode-segmentation` (`1.13.x`, `docs.rs/unicode-segmentation`) for UAX #29 grapheme clusters (replacing `Intl.Segmenter`).
- **Mandatory overrides in `clusterWidth()`** (`packages/tui/src/utils.ts#L264-L321`):
  1. `terminalSpacingMarkRegex`: `Spacing_Mark` minus `U+1734, U+302E, U+302F`, plus legacy `wcwidth` exceptions `U+065F, U+0F7F, U+102B, U+102C, U+1031, U+1033..U+1035, U+1038, U+103A..U+103E`.
  2. `\p{RGI_Emoji}` -> width `2` (Rust `regex` lacks `\p{RGI_Emoji}` and `/v` set difference `--`; port an explicit RGI emoji table/matcher or verified emoji crate).
  3. Isolated regional indicators `U+1F1E6..U+1F1FF` -> width `2`.
  4. Multi-codepoint accumulation for trailing halfwidth/fullwidth (`U+FF00..U+FFEF`), Thai/Lao AM (`U+0E33`, `U+0EB3`), and post-mark spacing codepoints.
  5. Tabs `\t` are fixed width `3` (`utils.ts#L255,L346-L348`); `normalizeTerminalOutput` (`#L528-L550`) decomposes `U+0E33`/`U+0EB3` and expands visible `\t` to `"   "` while skipping ANSI escape sequences.

### `marked` (`18.0.11`) + `marked-cjk-friendly` (`0.1.2`) -> `pulldown-cmark` (`docs.rs/pulldown-cmark`) or Custom Ported Lexer
- **Upstream usage**: `packages/tui/src/components/markdown.ts#L1-L35,L230-L237,L396-L883`. Uses `marked` **strictly as a lexer/tokenizer** (never HTML emission):
  - Subclasses `Tokenizer` as `MarkdownTokenizer`: overrides `url(src)` (trims trailing `cjkPunctuationRegex`) and `del(src)` (strict `~~` regex).
  - Applies `markedCjkFriendly().tokenizer.emStrong` (`#L235`) for CJK flanking emphasis rules (`**结论**。` etc.).
  - Registers streaming `LATEX_MARKDOWN_EXTENSIONS` (`latexBlock`, inline `latex`).
  - Preprocesses with `trimPartialClosingFences` (`#L175-L198`), `removeHtmlCommentBlocks` collapsing adjacent `space` tokens (`#L212-L228`), and checks `tokens.links` (`#L404`) to disable block caching.
- **Rust risk & recommendation**: Standard `pulldown-cmark` implements CommonMark/GFM delimiter flanking rules that diverge from `marked-cjk-friendly` on zero-space full-width CJK punctuation boundaries, and does not natively expose `marked`'s custom `url`/`del`/`latex` tokenizer extensions. Either wrap a custom inline CJK emphasis + LaTeX tokenizer pass around `pulldown-cmark`, or port the focused Markdown block/inline lexer directly so every `packages/tui/test/markdown.test.ts` and `latex.test.ts` case passes identically.

## 2. Native Platform & Clipboard Layer (`packages/tui/native/`)

Shared trait surface (`packages/tui/src/native-platform.ts#L7-L23`, `native/clipboard.h#L10-L30`):
- `ModifierKey`: `Shift | Command | Control | Option`
- Async `NativeClipboard`: `get_text()`, `get_image()`, optional `get_file_paths()`, optional `set_text(text)`
- Sync `NativePlatformHelper`: `enable_virtual_terminal_input()`, `is_modifier_pressed(ModifierKey)`

| Platform | Upstream Native Source | Rust Replacement (`#[cfg(...)]` isolated) | Key Behavioral Invariants |
| --- | --- | --- | --- |
| **Linux (X11)** | `native/linux/src/linux-platform-x11.c`, `clipboard-worker.h` (`libxcb.so.1`) | Pure-Rust **`x11rb`** (`docs.rs/x11rb`) on a dedicated worker thread with 3s deadline (`std::sync::Condvar` / timeout); zero C `libxcb` linking! | Active only when `DISPLAY` is set. Exports `get_text` and `get_image` only (no `set_text`, no `get_file_paths`). Hidden 1x1 window; supports `CLIPBOARD`, `TARGETS`, `INCR` up to **50 MiB** (`MAX_CLIPBOARD_BYTES`) with 2000ms X11 poll deadline. Text priority: `text/plain;charset=utf-8` -> `UTF-8` -> `UTF8_STRING` -> `text/plain` -> `STRING` (**Latin-1** decoded to UTF-8, `#L264`). Image priority: `png`, `jpeg`, `webp`, `gif`, `bmp`, `tiff`. Stalled worker marks `worker_busy` and returns `None` until thread completes. No modifier query on Linux (`is_modifier_pressed` -> `false`). |
| **Linux (Wayland & writes)** | Shell fallback in `packages/coding-agent/src/utils/clipboard*.ts` | Same subprocess fallback (`wl-paste`/`wl-copy` when `WAYLAND_DISPLAY` set, `xclip`/`xsel` fallback) + OSC 52 | Matches upstream `native/linux/README.md#L3-L5`. |
| **macOS** | `native/darwin/src/darwin-platform.m` (`AppKit` / `CoreGraphics`) | **`objc2-app-kit`** + `core-graphics` (`docs.rs/objc2-app-kit`) behind `#[cfg(target_os = "macos")]` | `CGEventSourceFlagsState(CombinedSessionState)` for Shift/Command/Control/Option; `NSPasteboard::generalPasteboard()` for UTF-8 text, PNG/TIFF->PNG images, and file URLs (`NSPasteboardURLReadingFileURLsOnlyKey`). |
| **Windows** | `native/win32/src/win32-platform.c` (`kernel32` + `user32`) | **`windows-sys`** (already used in `packages/env/daemon/Cargo.toml`) behind `#[cfg(windows)]` | `SetConsoleMode(STD_INPUT_HANDLE, mode \| ENABLE_VIRTUAL_TERMINAL_INPUT)` called **after** `set_raw_mode(true)` (`terminal.ts#L229-L233`); `GetAsyncKeyState` (`& 0x8000`) for modifiers; `OpenClipboard` retried 10x5ms; `CF_UNICODETEXT` UTF-16 read/write via hidden `HWND_MESSAGE` window; `get_image` prefers `"PNG"` then `CF_DIBV5`/`CF_DIB` with 14-byte `'BM'` `BITMAPFILEHEADER` synthesis (`win32-platform.c#L108-L171`). |

## 3. Terminal Protocols & Raw I/O

- **Raw TTY & Signals**: `rustix` / `libc` `termios` (`#[cfg(unix)]`) + `windows-sys` (`#[cfg(windows)]`) or `crossterm` low-level raw mode (keeping direct escape emission so `TuiMainScreen` and `TuiAltScreen` emit byte-identical sequences). Note: `restoreOnExit` (`terminal.ts#L543-L545`) writes synchronously to stdout fd so exit cleanup is never dropped.
- **Synchronized output**: CSI 2026 (`\x1b[?2026h` ... `\x1b[?2026l`).
- **Kitty keyboard protocol**: Startup sends `\x1b[>7u\x1b[?u` + OSC 7501 query + DA1 `\x1b[c`; activates Kitty mode on `\x1b[?<flags>u` (`flags != 0`), else falls back to xterm `modifyOtherKeys` level 2 (`\x1b[>4;2m`, disabled `\x1b[>4;0m`); pops `\x1b[<u` on exit (`terminal.ts#L19-L39,L302-L344,L449`).
- **Inline images**: Cell-size query `\x1b[16t` -> `\x1b[6;<h>;<w>t`; Kitty (`a=T,f=100,q=2`, 4096-byte base64 chunks, `a=p` reuse, `a=d` delete, emitted *after* row text/clears in `TuiAltScreen`); iTerm2 (`\x1b]1337;File=inline=1;...`).
- **OSC 133 prompt markers**: Parsed from `scrollContentLines` for `scrollToPrompt()` in `TuiAltScreen`, stripped from all emitted output (`layout.ts#L14,L345`, `tui-main-screen.ts#L9`, `tui-alt-screen.ts#L74`).
- **Mouse / focus / paste / color queries**: Alt-screen enables `?1000h`, `?1002h`, `?1004h`, `?1006h` (+ `?1003h` outside multiplexers), SGR + legacy X10 wheel decoding, `WheelScrollAccelerator` (disabled on local macOS), bracketed paste `?2004h`/`l`, OSC 8 hyperlinks, OSC 0 title, OSC 9;4 progress, OSC 7501 status (`<= 2048` UTF-8 bytes truncated on char boundary before base64), OSC 10/11/4 + `?2031h`/`?997;1/2n` color-scheme queries, internal APC cursor markers (`\x1b_pi:c\x07`, `\x1b_pi:fc\x07`, `\x1b_pi:/fc\x07`).

## 4. Semantic Traps

1. **`ansiCodeLength` (`utils.ts#L587-L616`)**: Terminates CSI only on `m, G, K, H, J` (not all `0x40..0x7E` final bytes). `AnsiCodeTracker` preserves OSC 8's exact `\x07` vs `\x1b\\` terminator across line wraps and keeps OSC 8 active across `\x1b[0m` (`#L694-L702,L838`).
2. **`cjkPunctuationRegex` (`utils.ts#L58-L61`)**: Uses lookahead `(?=\p{Punctuation})`, unsupported by Rust `regex`; implement as a `char` predicate or `fancy-regex`.
3. **OKLCH / OKHSL math (`oklab.ts`, `colors.ts`)**: Hue wrapping `((h % 360) + 360) % 360` -> `.rem_euclid(360.0)`; JS `Math.round` rounds half toward `+∞` (not `f64::round` half-away-from-zero); avoid formatting `"-0"`; reject `NaN`/`Infinity`.
4. **Global mutable caches in JS**: `keys.ts` (`_kittyProtocolActive`, `_lastEventType`), `terminal-image.ts` (LRU-1000 `kittyImageMetadata`, `cellDimensions`), and `utils.ts` (FIFO-512 `widthCache`) should live in explicit structs or synchronized state in Rust.
5. **UTF-16 vs UTF-8**: `BoundedTerminalWriter` and editor cursor offsets operate on UTF-16 code units in TS; keep `utf16.rs` conversions on `rust-anything` bridge boundaries and char boundaries internally.
