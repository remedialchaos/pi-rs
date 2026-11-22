// Terminal user interface components with differential rendering, ported from `@earendil-works/pi-tui`.
//
// The module map and the frozen interfaces are in `PORTING.md` next to this file.

pub mod alt_screen_search;
pub mod autocomplete;
pub mod colors;
pub mod components;
pub mod editor_component;
pub mod fuzzy;
pub mod keybindings;
pub mod keys;
pub mod kill_ring;
pub mod latex;
pub mod layout;
pub mod layout_node;
pub mod native_platform;
pub mod oklab;
pub mod program_status;
pub mod stdin_buffer;
pub mod terminal;
pub mod terminal_colors;
pub mod terminal_image;
pub mod tui;
pub mod tui_alt_screen;
pub mod tui_main_screen;
pub mod undo_stack;
pub mod utils;
pub mod wheel_scroll;
pub mod word_navigation;

pub use autocomplete::{
    AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions, CombinedAutocompleteProvider,
    SlashCommand,
};
pub use colors::{
    Color, ColorInput, ColorMixSpace, OkhslChannels, OklchColorValue, RgbColorValue,
    TerminalColorMode, TextAttributes, TextStyle, background_ansi, color_to_hex, color_to_okhsl,
    color_to_oklch, color_to_rgb, foreground_ansi, indexed_color, mix_colors, okhsl_color,
    oklch_color, parse_color, rgb_color, style_text, style_text_with_ansi,
};
pub use components::{
    r#box::Box,
    cancellable_loader::CancellableLoader,
    editor::{Editor, EditorOptions, EditorTheme},
    h_stack::HStack,
    image::{Image, ImageOptions, ImageTheme, ImageTranscoder, set_image_transcoder},
    input::{Input, InputOptions},
    loader::{Loader, LoaderIndicatorOptions},
    markdown::{DefaultTextStyle, Markdown, MarkdownOptions, MarkdownTheme},
    mouse_region::{MouseRegion, MouseRegionHandler},
    scroll_view::{ScrollView, ScrollViewOptions, Scrollbar},
    select_list::{
        SelectItem, SelectList, SelectListLayoutOptions, SelectListTheme, TruncatePrimaryContext,
    },
    settings_list::{SettingItem, SettingsList, SettingsListOptions, SettingsListTheme},
    spacer::Spacer,
    stack::{StackEntryOptions, StackOptions},
    text::Text,
    truncated_text::TruncatedText,
    v_stack::VStack,
};
pub use editor_component::{
    EditorAppearance, EditorAutocomplete, EditorComponent, EditorHistory, EditorInsertion,
};
pub use fuzzy::{FuzzyMatch, fuzzy_filter, fuzzy_match};
pub use keybindings::{
    Keybinding, KeybindingConflict, KeybindingDefinition, KeybindingDefinitions, KeybindingsConfig,
    KeybindingsManager, TUI_KEYBINDINGS, get_keybindings, get_select_page_index, set_keybindings,
};
pub use keys::{
    Key, KeyEventType, KeyId, decode_kitty_printable, is_key_release, is_key_repeat,
    is_kitty_protocol_active, matches_key, parse_key, set_kitty_protocol_active,
};
pub use latex::{RenderLatexOptions, render_latex};
pub use native_platform::{NativeClipboard, get_native_clipboard};
pub use oklab::oklab_to_okhsl_lightness;
pub use program_status::{
    ProgramStatus, ProgramStatusKind, ProgramStatusState, format_program_status,
};
pub use stdin_buffer::{StdinBuffer, StdinBufferOptions, StdinEvent};
pub use terminal::{ProcessTerminal, Terminal, is_apple_terminal_session};
pub use terminal_colors::{
    RgbColor, TerminalColorScheme, TerminalColors, parse_terminal_color_scheme_report,
};
pub use terminal_image::{
    CellDimensions, ImageDimensions, ImageProtocol, ImageRenderOptions, TerminalCapabilities,
    allocate_image_id, calculate_image_rows, delete_all_kitty_images, delete_kitty_image,
    detect_capabilities, encode_iterm2, encode_kitty, get_capabilities, get_cell_dimensions,
    get_gif_dimensions, get_image_dimensions, get_jpeg_dimensions, get_png_dimensions,
    get_terminal_color_mode, get_webp_dimensions, hyperlink, image_fallback, render_image,
    reset_capabilities_cache, set_capabilities, set_capability_overrides, set_cell_dimensions,
};
pub use tui::{
    CURSOR_MARKER, Component, ComponentRef, Container, Focusable, OverlayAnchor, OverlayBounds,
    OverlayHandle, OverlayMargin, OverlayOptions, OverlayUnfocusOptions, SizeValue, Tui,
    TuiInputListener, TuiInputListenerResult, TuiMode, TuiMouseButton, TuiMouseEvent,
    TuiMouseEventResult, TuiMouseEventType, TuiStopOptions, ViewportTui, composite_tui_line,
    render_fake_cursor,
};
pub use tui_alt_screen::{TuiAltScreen, TuiAltScreenOptions};
pub use tui_main_screen::{TuiMainScreen, TuiMainScreenRenderState};
pub use utils::{
    get_osc8_link_at_column, replace_control_characters, slice_by_column, strip_terminal_sequences,
    truncate_to_width, visible_width, wrap_text_with_ansi,
};
pub use wheel_scroll::WheelScrollLines;
