//! Terminal capabilities and Kitty/iTerm2 image placement.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::hash::{BuildHasher, RandomState};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::STANDARD};
use indexmap::IndexMap;
use regex::Regex;

use crate::{colors::TerminalColorMode, utils::visible_width};

const KITTY: &str = "\x1b_G";
const ITERM: &str = "\x1b]1337;File=";
const ST: &str = "\x1b\\";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageProtocol {
    Kitty,
    Iterm2,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TerminalCapabilities {
    pub images: Option<ImageProtocol>,
    pub true_color: bool,
    pub hyperlinks: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CapabilityOverrides {
    pub images: Option<Option<ImageProtocol>>,
    pub true_color: Option<bool>,
    pub hyperlinks: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellDimensions {
    pub width_px: usize,
    pub height_px: usize,
}

impl Default for CellDimensions {
    fn default() -> Self {
        Self {
            width_px: 9,
            height_px: 18,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageDimensions {
    pub width_px: usize,
    pub height_px: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ImageRenderOptions {
    pub max_width_cells: Option<usize>,
    pub max_height_cells: Option<usize>,
    pub preserve_aspect_ratio: Option<bool>,
    pub image_id: Option<u32>,
    pub move_cursor: Option<bool>,
}

#[derive(Default)]
struct State {
    capabilities: Option<TerminalCapabilities>,
    overrides: CapabilityOverrides,
    cell: CellDimensions,
    kitty: IndexMap<u32, (KittyImageMetadata, u64)>,
    generation: u64,
    max_rows: usize,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::default()));

fn state() -> MutexGuard<'static, State> {
    STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub fn get_cell_dimensions() -> CellDimensions {
    state().cell
}
pub fn set_cell_dimensions(cell: CellDimensions) {
    state().cell = cell;
}

fn probe_tmux_hyperlinks() -> bool {
    let Ok(mut child) = Command::new("tmux")
        .args(["display-message", "-p", "#{client_termfeatures}"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let end = Instant::now() + Duration::from_millis(250);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                return child.wait_with_output().ok().is_some_and(|output| {
                    String::from_utf8_lossy(&output.stdout)
                        .split(',')
                        .any(|s| s.trim() == "hyperlinks")
                });
            }
            Ok(None) if Instant::now() < end => std::thread::park_timeout(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

pub fn detect_capabilities() -> TerminalCapabilities {
    detect_capabilities_with(
        |key| std::env::var(key).ok(),
        cfg!(windows),
        probe_tmux_hyperlinks,
    )
}

pub fn detect_capabilities_with(
    env: impl Fn(&str) -> Option<String>,
    windows: bool,
    tmux_hyperlinks: impl FnOnce() -> bool,
) -> TerminalCapabilities {
    let lower = |key| env(key).unwrap_or_default().to_ascii_lowercase();
    let has = |key| env(key).is_some_and(|s| !s.is_empty());
    let flag = |key| match env(key).as_deref() {
        Some("1") => Some(true),
        Some("0") => Some(false),
        _ => None,
    };
    let prog = lower("TERM_PROGRAM");
    let term = lower("TERM");
    let hint =
        matches!(lower("COLORTERM").as_str(), "truecolor" | "24bit") || term.ends_with("-direct");
    let override_link = flag("PI_HYPERLINKS");
    let supported = |images| TerminalCapabilities {
        images,
        true_color: true,
        hyperlinks: true,
    };
    let mut caps = if has("TMUX") || term.starts_with("tmux") {
        TerminalCapabilities {
            images: None,
            true_color: hint,
            hyperlinks: override_link.unwrap_or_else(tmux_hyperlinks),
        }
    } else if term.starts_with("screen") {
        TerminalCapabilities {
            true_color: hint,
            ..TerminalCapabilities::default()
        }
    } else if prog == "herdr" {
        TerminalCapabilities {
            images: Some(ImageProtocol::Kitty),
            true_color: hint,
            hyperlinks: true,
        }
    } else if prog == "rex"
        || (prog == "orca" && env("ORCA_IMAGE_PROTOCOL").as_deref() == Some("kitty"))
        || prog == "kitty"
        || has("KITTY_WINDOW_ID")
        || prog == "ghostty"
        || term.contains("ghostty")
        || has("GHOSTTY_RESOURCES_DIR")
        || prog == "wezterm"
        || has("WEZTERM_PANE")
        || prog == "warpterminal"
        || has("WARP_SESSION_ID")
        || has("WARP_TERMINAL_SESSION_UUID")
    {
        supported(Some(ImageProtocol::Kitty))
    } else if prog == "iterm.app" || has("ITERM_SESSION_ID") {
        supported(Some(ImageProtocol::Iterm2))
    } else if has("WT_SESSION")
        || matches!(prog.as_str(), "alacritty" | "vscode" | "zed")
        || term.starts_with("alacritty")
        || has("ALACRITTY_WINDOW_ID")
    {
        supported(None)
    } else {
        TerminalCapabilities {
            true_color: windows || lower("TERMINAL_EMULATOR") == "jetbrains-jediterm" || hint,
            ..TerminalCapabilities::default()
        }
    };
    caps.images = match lower("PI_IMAGE_PROTOCOL").as_str() {
        "kitty" => Some(ImageProtocol::Kitty),
        "iterm2" => Some(ImageProtocol::Iterm2),
        "none" | "0" => None,
        _ => caps.images,
    };
    caps.true_color = flag("PI_TRUE_COLOR").unwrap_or(caps.true_color);
    caps.hyperlinks = override_link.unwrap_or(caps.hyperlinks);
    caps
}

pub fn get_capabilities() -> TerminalCapabilities {
    let mut state = state();
    if let Some(caps) = state.capabilities {
        return caps;
    }
    let overrides = state.overrides;
    let mut caps = detect_capabilities_with(
        |key| std::env::var(key).ok(),
        cfg!(windows),
        || overrides.hyperlinks.unwrap_or_else(probe_tmux_hyperlinks),
    );
    caps.images = overrides.images.unwrap_or(caps.images);
    caps.true_color = overrides.true_color.unwrap_or(caps.true_color);
    caps.hyperlinks = overrides.hyperlinks.unwrap_or(caps.hyperlinks);
    state.capabilities = Some(caps);
    caps
}

pub fn get_terminal_color_mode(caps: TerminalCapabilities) -> TerminalColorMode {
    if caps.true_color {
        TerminalColorMode::Truecolor
    } else {
        TerminalColorMode::Color256
    }
}

pub fn reset_capabilities_cache() {
    state().capabilities = None;
}
pub fn set_capabilities(caps: TerminalCapabilities) {
    state().capabilities = Some(caps);
}
pub fn set_capability_overrides(overrides: CapabilityOverrides) {
    let mut state = state();
    if state.overrides != overrides {
        state.overrides = overrides;
        state.capabilities = None;
    }
}

pub fn is_image_line(line: &str) -> bool {
    line.contains(KITTY) || line.contains(ITERM)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageLineExtent {
    pub first_row: isize,
    pub last_row: isize,
    pub start_col: usize,
    /// `None` means the protocol does not express the width in cells.
    pub end_col: Option<usize>,
}

pub fn get_image_line_extent(line: &str) -> Option<ImageLineExtent> {
    if let Some(start) = line.find(KITTY) {
        let controls = kitty_controls(&line[start..]).map_or("", |(_, controls)| controls);
        let columns = kitty_value(controls, "c")
            .and_then(|c| c.parse::<usize>().ok())
            .or_else(|| kitty_metadata(controls).map(|(metadata, _)| metadata.columns));
        let start_col = visible_width(&line[..start]);
        return Some(ImageLineExtent {
            first_row: 0,
            last_row: get_kitty_image_placement_rows(line)
                .unwrap_or(1)
                .saturating_sub(1) as isize,
            start_col,
            end_col: columns.map(|width| start_col.saturating_add(width)),
        });
    }
    let (prefix, rest) = line.split_once(ITERM)?;
    let mut up = 0;
    let mut before = prefix;
    if let Some((body, value)) = prefix.rsplit_once("\x1b[")
        && let Some(rows) = value
            .strip_suffix('A')
            .and_then(|n| n.parse::<isize>().ok())
            .filter(|n| *n >= 0)
    {
        up = rows;
        before = body;
    }
    let columns = rest
        .split(':')
        .next()?
        .split(';')
        .find_map(|part| part.strip_prefix("width=")?.parse::<usize>().ok());
    let start_col = visible_width(before);
    Some(ImageLineExtent {
        first_row: -up,
        last_row: 0,
        start_col,
        end_col: columns.map(|width| start_col.saturating_add(width)),
    })
}

pub fn strip_image_sequences(line: &str) -> Cow<'_, str> {
    static IMAGES: LazyLock<Regex> = LazyLock::new(|| {
        // INFALLIBLE: A fixed Kitty and iTerm2 escape-sequence grammar.
        Regex::new(r"\x1b_G[^\x1b]*\x1b\\|(?:\x1b\[[0-9]+A)?\x1b\]1337;File=[^\x07]*\x07")
            .expect("valid image sequence regex")
    });
    if is_image_line(line) {
        IMAGES.replace_all(line, "")
    } else {
        Cow::Borrowed(line)
    }
}

pub fn allocate_image_id() -> u32 {
    (RandomState::new().hash_one(()) % u64::from(u32::MAX - 1)) as u32 + 1
}

#[derive(Clone, Copy, Debug, Default)]
pub struct KittyOptions {
    pub columns: Option<usize>,
    pub rows: Option<usize>,
    pub image_id: Option<u32>,
    pub move_cursor: Option<bool>,
}

pub fn encode_kitty(base64: &str, options: KittyOptions) -> String {
    const CHUNK: usize = 4096;
    let mut sequence = String::with_capacity(base64.len() + 60 + (base64.len() / CHUNK) * 12);
    sequence.push_str("\x1b_Ga=T,f=100,q=2");
    if options.move_cursor == Some(false) {
        sequence.push_str(",C=1");
    }
    if let Some(n) = options.columns.filter(|&n| n != 0) {
        let _ = write!(sequence, ",c={n}");
    }
    if let Some(n) = options.rows.filter(|&n| n != 0) {
        let _ = write!(sequence, ",r={n}");
    }
    if let Some(n) = options.image_id.filter(|&n| n != 0) {
        let _ = write!(sequence, ",i={n}");
    }
    let mut rest = base64;
    if rest.len() > CHUNK {
        sequence.push_str(",m=1;");
        while rest.len() > CHUNK {
            let mut end = CHUNK;
            while !rest.is_char_boundary(end) {
                end -= 1;
            }
            sequence.push_str(&rest[..end]);
            sequence.push_str(ST);
            rest = &rest[end..];
            sequence.push_str(if rest.len() > CHUNK {
                "\x1b_Gm=1;"
            } else {
                "\x1b_Gm=0;"
            });
        }
    } else {
        sequence.push(';');
    }
    sequence.push_str(rest);
    sequence.push_str(ST);
    sequence
}

pub fn delete_kitty_image(id: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={id},q=2{ST}")
}
pub fn delete_all_kitty_images() -> &'static str {
    "\x1b_Ga=d,d=A,q=2\x1b\\"
}
pub fn delete_all_kitty_placements() -> &'static str {
    "\x1b_Ga=d,d=a,q=2\x1b\\"
}

#[derive(Clone, Copy, Debug)]
pub enum ItermSize<'a> {
    Cells(usize),
    Value(&'a str),
}

impl std::fmt::Display for ItermSize<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cells(n) => n.fmt(f),
            Self::Value(s) => f.write_str(s),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Iterm2Options<'a> {
    pub width: Option<ItermSize<'a>>,
    pub height: Option<ItermSize<'a>>,
    pub name: Option<&'a str>,
    pub preserve_aspect_ratio: Option<bool>,
    pub inline: Option<bool>,
}

pub fn encode_iterm2(base64: &str, options: Iterm2Options<'_>) -> String {
    let n = base64.encode_utf16().count();
    let padding = base64
        .bytes()
        .rev()
        .take(2)
        .take_while(|&b| b == b'=')
        .count();
    let mut out = String::with_capacity(base64.len() + 100);
    let _ = write!(
        out,
        "{ITERM}inline={};size={}",
        u8::from(options.inline != Some(false)),
        (n - padding) * 3 / 4
    );
    if let Some(width) = options.width {
        let _ = write!(out, ";width={width}");
    }
    if let Some(height) = options.height {
        let _ = write!(out, ";height={height}");
    }
    if let Some(name) = options.name.filter(|s| !s.is_empty()) {
        let _ = write!(out, ";name={}", STANDARD.encode(name));
    }
    if options.preserve_aspect_ratio == Some(false) {
        out.push_str(";preserveAspectRatio=0");
    }
    out.push(':');
    out.push_str(base64);
    out.push('\x07');
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageCellSize {
    pub columns: usize,
    pub rows: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KittyImageMetadata {
    pub image_id: u32,
    pub columns: usize,
    pub rows: usize,
    pub width_px: usize,
    pub height_px: usize,
}

pub fn register_kitty_image_metadata(metadata: KittyImageMetadata) {
    let mut state = state();
    state.generation = state.generation.wrapping_add(1);
    state.max_rows = state.max_rows.max(metadata.rows);
    state.kitty.shift_remove(&metadata.image_id);
    let generation = state.generation;
    state
        .kitty
        .insert(metadata.image_id, (metadata, generation));
    if state.kitty.len() > 1000 {
        state.kitty.shift_remove_index(0);
    }
}

pub fn get_max_kitty_image_rows() -> usize {
    state().max_rows
}

fn kitty_controls(line: &str) -> Option<(usize, &str)> {
    let start = line.find(KITTY)?;
    Some((start, line[start + KITTY.len()..].split_once(';')?.0))
}

fn kitty_value<'a>(controls: &'a str, key: &str) -> Option<&'a str> {
    controls.split(',').find_map(|control| {
        let (tag, value) = control.split_once('=')?;
        (tag == key).then_some(value)
    })
}

fn kitty_metadata(controls: &str) -> Option<(KittyImageMetadata, u64)> {
    let id = kitty_value(controls, "i")?.parse::<u32>().ok()?;
    state().kitty.get(&id).copied()
}

fn explicit_kitty_rows(controls: &str) -> Option<usize> {
    kitty_value(controls, "r")?
        .parse::<usize>()
        .ok()
        .filter(|&rows| rows != 0)
}

pub fn get_kitty_image_metadata(line: &str) -> Option<KittyImageMetadata> {
    kitty_metadata(kitty_controls(line)?.1).map(|(metadata, _)| metadata)
}

pub fn get_kitty_image_placement_rows(line: &str) -> Option<usize> {
    let (_, controls) = kitty_controls(line)?;
    explicit_kitty_rows(controls)
        .or_else(|| kitty_metadata(controls).map(|(metadata, _)| metadata.rows))
}

#[derive(Debug, PartialEq, Eq)]
pub struct KittyImagePlacement {
    pub image_id: u32,
    pub transmission_generation: u64,
    pub transmission_bytes: usize,
    pub estimated_decoded_bytes: usize,
    pub rows: usize,
    pub sequence: String,
    pub replacement_line: String,
}

pub fn get_kitty_image_placement(line: &str) -> Option<KittyImagePlacement> {
    let (start, controls) = kitty_controls(line)?;
    let (metadata, generation) = kitty_metadata(controls)?;
    let mut rest = &line[start..];
    loop {
        let (_, command_controls) = kitty_controls(rest)?;
        let end = rest.find(ST)? + ST.len();
        rest = &rest[end..];
        if kitty_value(command_controls, "m") != Some("1") {
            break;
        }
        if !rest.starts_with(KITTY) {
            return None;
        }
    }
    let mut sequence = String::from("\x1b_Ga=p,q=2,");
    let mut first = true;
    for control in controls.split(',').filter(|control| {
        matches!(
            control.split('=').next().unwrap_or(""),
            "i" | "p"
                | "x"
                | "y"
                | "w"
                | "h"
                | "X"
                | "Y"
                | "c"
                | "r"
                | "C"
                | "U"
                | "z"
                | "P"
                | "Q"
                | "H"
                | "V"
        )
    }) {
        if !first {
            sequence.push(',');
        }
        sequence.push_str(control);
        first = false;
    }
    sequence.push_str(ST);
    let replacement_line = format!("{}{sequence}{rest}", &line[..start]);
    Some(KittyImagePlacement {
        image_id: metadata.image_id,
        transmission_generation: generation,
        transmission_bytes: line.len() - rest.len() - start,
        estimated_decoded_bytes: metadata
            .width_px
            .saturating_mul(metadata.height_px)
            .saturating_mul(4),
        rows: explicit_kitty_rows(controls).unwrap_or(metadata.rows),
        sequence,
        replacement_line,
    })
}

pub fn crop_kitty_image_line(line: &str, hidden_rows: usize, visible_rows: usize) -> Cow<'_, str> {
    let Some((start, controls)) = kitty_controls(line) else {
        return Cow::Borrowed(line);
    };
    let Some((metadata, _)) = kitty_metadata(controls) else {
        return Cow::Borrowed(line);
    };
    if hidden_rows >= metadata.rows || visible_rows == 0 {
        return Cow::Borrowed(line);
    }
    let rows = visible_rows.min(metadata.rows - hidden_rows);
    if hidden_rows == 0 && rows == metadata.rows {
        return Cow::Borrowed(line);
    }
    let rows_total = metadata.rows as u128;
    let height = metadata.height_px as u128;
    let y = (height * hidden_rows as u128) / rows_total;
    let end = (height * (hidden_rows + rows) as u128).div_ceil(rows_total);
    let h = end.min(height).saturating_sub(y).max(1);
    let mut output = String::with_capacity(line.len() + 20);
    output.push_str(&line[..start + KITTY.len()]);
    for control in controls
        .split(',')
        .filter(|s| !s.starts_with("y=") && !s.starts_with("h=") && !s.starts_with("r="))
    {
        output.push_str(control);
        output.push(',');
    }
    let _ = write!(output, "y={y},h={h},r={rows}");
    output.push_str(&line[start + KITTY.len() + controls.len()..]);
    Cow::Owned(output)
}

pub fn calculate_image_cell_size(
    image: ImageDimensions,
    max_width: usize,
    max_height: Option<usize>,
    cell: CellDimensions,
    optimize_aspect_ratio: bool,
) -> ImageCellSize {
    let (max_width, max_height) = (max_width.max(1), max_height.map(|h| h.max(1)));
    let (width, height) = (image.width_px.max(1) as f64, image.height_px.max(1) as f64);
    let (cell_w, cell_h) = (cell.width_px.max(1) as f64, cell.height_px.max(1) as f64);
    let w_scale = (max_width as f64) * cell_w / width;
    let h_scale = max_height.map_or(w_scale, |max| (max as f64) * cell_h / height);
    let scale = w_scale.min(h_scale);
    let mut columns = ((width * scale / cell_w).ceil() as usize).clamp(1, max_width);
    let mut rows =
        ((height * scale / cell_h).ceil() as usize).clamp(1, max_height.unwrap_or(usize::MAX));
    if optimize_aspect_ratio {
        let choose = |n: usize, ideal: f64| {
            if n <= 1 {
                return n;
            }
            let distortion = |n| f64::max((n as f64) / ideal, ideal / (n as f64));
            if distortion(n - 1) < distortion(n) {
                n - 1
            } else {
                n
            }
        };
        if w_scale <= h_scale {
            rows = choose(rows, columns as f64 * cell_w * height / (width * cell_h));
        } else {
            columns = choose(columns, rows as f64 * cell_h * width / (height * cell_w));
        }
    }
    ImageCellSize { columns, rows }
}

pub fn calculate_image_rows(
    image: ImageDimensions,
    target_width: usize,
    cell: CellDimensions,
) -> usize {
    calculate_image_cell_size(image, target_width, None, cell, false).rows
}

/// Decode bytes lazily: dimension probing never needs the image pixel payload.
fn decoded(base64: &str) -> impl Iterator<Item = u8> + '_ {
    let mut input = base64.bytes().take_while(|&b| b != b'=');
    let mut saved = 0_u16;
    let mut bits = 0;
    std::iter::from_fn(move || {
        for byte in input.by_ref() {
            let digit = match byte {
                b'A'..=b'Z' => byte - b'A',
                b'a'..=b'z' => byte - b'a' + 26,
                b'0'..=b'9' => byte - b'0' + 52,
                b'+' | b'-' => 62,
                b'/' | b'_' => 63,
                _ => continue,
            };
            saved = (saved << 6) | u16::from(digit);
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                return Some((saved >> bits) as u8);
            }
        }
        None
    })
}

fn header<const N: usize>(base64: &str) -> Option<[u8; N]> {
    let mut header = [0; N];
    let mut bytes = decoded(base64);
    for byte in &mut header {
        *byte = bytes.next()?;
    }
    Some(header)
}

pub fn get_png_dimensions(base64: &str) -> Option<ImageDimensions> {
    let b = header::<24>(base64)?;
    (b[..4] == [0x89, b'P', b'N', b'G']).then(|| ImageDimensions {
        width_px: u32::from_be_bytes([b[16], b[17], b[18], b[19]]) as usize,
        height_px: u32::from_be_bytes([b[20], b[21], b[22], b[23]]) as usize,
    })
}

pub fn get_jpeg_dimensions(base64: &str) -> Option<ImageDimensions> {
    let mut bytes = decoded(base64);
    if [bytes.next()?, bytes.next()?] != [0xff, 0xd8] {
        return None;
    }
    while let Some(byte) = bytes.next() {
        if byte != 0xff {
            continue;
        }
        let marker = bytes.next()?;
        let length = u16::from_be_bytes([bytes.next()?, bytes.next()?]);
        if (0xc0..=0xc2).contains(&marker) {
            bytes.next()?;
            let height = u16::from_be_bytes([bytes.next()?, bytes.next()?]);
            let width = u16::from_be_bytes([bytes.next()?, bytes.next()?]);
            bytes.next()?;
            return Some(ImageDimensions {
                width_px: usize::from(width),
                height_px: usize::from(height),
            });
        }
        if length < 2 {
            return None;
        }
        for _ in 2..length {
            bytes.next()?;
        }
    }
    None
}

pub fn get_gif_dimensions(base64: &str) -> Option<ImageDimensions> {
    let b = header::<10>(base64)?;
    let signature = [b[0], b[1], b[2], b[3], b[4], b[5]].map(|n| n & 0x7f);
    matches!(&signature, b"GIF87a" | b"GIF89a").then(|| ImageDimensions {
        width_px: u16::from_le_bytes([b[6], b[7]]).into(),
        height_px: u16::from_le_bytes([b[8], b[9]]).into(),
    })
}

pub fn get_webp_dimensions(base64: &str) -> Option<ImageDimensions> {
    let b = header::<30>(base64)?;
    let signature = b.map(|n| n & 0x7f);
    if &signature[..4] != b"RIFF" || &signature[8..12] != b"WEBP" {
        return None;
    }
    let (width_px, height_px) = match &signature[12..16] {
        b"VP8 " => (
            u32::from(u16::from_le_bytes([b[26], b[27]]) & 0x3fff),
            u32::from(u16::from_le_bytes([b[28], b[29]]) & 0x3fff),
        ),
        b"VP8L" => {
            let bits = u32::from_le_bytes([b[21], b[22], b[23], b[24]]);
            ((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1)
        }
        b"VP8X" => (
            u32::from_le_bytes([b[24], b[25], b[26], 0]) + 1,
            u32::from_le_bytes([b[27], b[28], b[29], 0]) + 1,
        ),
        _ => return None,
    };
    Some(ImageDimensions {
        width_px: width_px as usize,
        height_px: height_px as usize,
    })
}

pub fn get_image_dimensions(base64: &str, mime_type: &str) -> Option<ImageDimensions> {
    match mime_type {
        "image/png" => get_png_dimensions(base64),
        "image/jpeg" => get_jpeg_dimensions(base64),
        "image/gif" => get_gif_dimensions(base64),
        "image/webp" => get_webp_dimensions(base64),
        _ => None,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct RenderedImage {
    pub sequence: String,
    pub columns: usize,
    pub rows: usize,
    pub image_id: Option<u32>,
}

pub fn render_image(
    base64: &str,
    image: ImageDimensions,
    options: ImageRenderOptions,
) -> Option<RenderedImage> {
    let protocol = get_capabilities().images?;
    let ImageCellSize { columns, rows } = calculate_image_cell_size(
        image,
        options.max_width_cells.unwrap_or(80),
        options.max_height_cells,
        get_cell_dimensions(),
        protocol == ImageProtocol::Kitty,
    );
    let (sequence, image_id) = match protocol {
        ImageProtocol::Kitty => {
            if let Some(image_id) = options.image_id {
                register_kitty_image_metadata(KittyImageMetadata {
                    image_id,
                    columns,
                    rows,
                    width_px: image.width_px,
                    height_px: image.height_px,
                });
            }
            (
                encode_kitty(
                    base64,
                    KittyOptions {
                        columns: Some(columns),
                        rows: Some(rows),
                        image_id: options.image_id,
                        move_cursor: options.move_cursor,
                    },
                ),
                options.image_id,
            )
        }
        ImageProtocol::Iterm2 => (
            encode_iterm2(
                base64,
                Iterm2Options {
                    width: Some(ItermSize::Cells(columns)),
                    height: Some(ItermSize::Value("auto")),
                    preserve_aspect_ratio: options.preserve_aspect_ratio,
                    ..Iterm2Options::default()
                },
            ),
            None,
        ),
    };
    Some(RenderedImage {
        sequence,
        columns,
        rows,
        image_id,
    })
}

pub fn hyperlink(text: &str, url: &str) -> String {
    format!("\x1b]8;;{url}{ST}{text}\x1b]8;;{ST}")
}

pub fn image_fallback(
    mime: &str,
    dimensions: Option<ImageDimensions>,
    filename: Option<&str>,
) -> String {
    let mut result = String::from("[Image: ");
    if let Some(filename) = filename.filter(|name| !name.is_empty()) {
        let display = std::env::home_dir()
            .and_then(|home| {
                let home = home.to_str()?;
                let rest = filename.strip_prefix(home)?;
                (rest.is_empty() || rest.starts_with(['/', '\\'])).then(|| format!("~{rest}"))
            })
            .map_or(Cow::Borrowed(filename), Cow::Owned);
        if get_capabilities().hyperlinks
            && let Ok(url) = url::Url::from_file_path(filename)
        {
            result.push_str(&hyperlink(&display, url.as_str()));
        } else {
            result.push_str(&display);
        }
        result.push(' ');
    }
    let _ = write!(result, "[{mime}]");
    if let Some(ImageDimensions {
        width_px,
        height_px,
    }) = dimensions
    {
        let _ = write!(result, " {width_px}x{height_px}");
    }
    result.push(']');
    result
}
