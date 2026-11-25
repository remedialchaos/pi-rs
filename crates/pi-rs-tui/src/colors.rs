//! Colors: the `Color` type, the color spaces and the ANSI escape sequences.
//!
//! Ported from `packages/tui/src/colors.ts`.

use crate::oklab;
use crate::oklab::js_round;

/// A color: a palette index, an RGB value or an OKLCH value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Color {
    /// A color of the 256 color ANSI palette.
    Indexed(u8),
    /// A color given by its sRGB channels.
    Rgb(RgbColorValue),
    /// A color given by its OKLCH channels.
    Oklch(OklchColorValue),
}

/// An RGB color.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RgbColorValue {
    /// The red channel, 0-255.
    pub r: f64,
    /// The green channel, 0-255.
    pub g: f64,
    /// The blue channel, 0-255.
    pub b: f64,
}

/// An OKLCH color.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OklchColorValue {
    /// The lightness, 0-1.
    pub l: f64,
    /// The chroma, not negative.
    pub c: f64,
    /// The hue in degrees, 0-360.
    pub h: f64,
}

/// OKHSL channels: hue in degrees, saturation and lightness 0-1. Saturation is relative to the most the
/// sRGB gamut allows at that hue and lightness, so every value is in gamut.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OkhslChannels {
    /// The hue in degrees.
    pub h: f64,
    /// The saturation, 0-1.
    pub s: f64,
    /// The lightness, 0-1.
    pub l: f64,
}

/// The attributes of a text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextAttributes {
    /// Bold.
    pub bold: bool,
    /// Dim.
    pub dim: bool,
    /// Italic.
    pub italic: bool,
    /// Underline.
    pub underline: bool,
    /// Inverse (swap the foreground and the background).
    pub inverse: bool,
    /// Strikethrough.
    pub strikethrough: bool,
}

/// The style of a text: attributes and colors.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TextStyle {
    /// The text attributes.
    pub attributes: TextAttributes,
    /// The foreground color.
    pub fg: Option<Color>,
    /// The background color.
    pub bg: Option<Color>,
}

/// The color capabilities of the terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalColorMode {
    /// The 256 color palette.
    Color256,
    /// 24 bit colors.
    Truecolor,
}

/// The color space a color mixing happens in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColorMixSpace {
    /// The OKLCH space, the default.
    #[default]
    Oklch,
    /// The sRGB space.
    Srgb,
}

/// A value to parse as a color: a text or a palette index.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ColorInput<'a> {
    /// A color written as text, e.g. `#ff0000`.
    Text(&'a str),
    /// A palette index.
    Index(f64),
}

/// Formats a number like JavaScript's `String(number)` does, for the error messages.
fn format_number(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if value == 0.0 {
        return "0".to_string();
    }
    // JavaScript writes the very large and the very small numbers in the exponential form.
    let magnitude = value.abs();
    if magnitude >= 1e21 || magnitude < 1e-6 {
        let exponential = format!("{value:e}");
        if let Some((mantissa, exponent)) = exponential.split_once('e')
            && !exponent.starts_with('-')
        {
            return format!("{mantissa}e+{exponent}");
        }
        return exponential;
    }
    value.to_string()
}

/// The number pattern of the upstream: `[+-]?(?:\d+(?:\.\d*)?|\.\d+)(?:e[+-]?\d+)?`.
fn parse_number(text: &str) -> Option<(f64, &str)> {
    let bytes = text.as_bytes();
    let mut index = 0;
    if index < bytes.len() && (bytes[index] == b'+' || bytes[index] == b'-') {
        index += 1;
    }
    let digits_start = index;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    let has_integer = index > digits_start;
    if index < bytes.len() && bytes[index] == b'.' {
        index += 1;
        let fraction_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        // `\d+(?:\.\d*)?` allows a dot without digits, `\.\d+` requires them.
        if !has_integer && index == fraction_start {
            return None;
        }
    } else if !has_integer {
        return None;
    }
    // The optional exponent.
    if index < bytes.len() && (bytes[index] == b'e' || bytes[index] == b'E') {
        let mut exponent = index + 1;
        if exponent < bytes.len() && (bytes[exponent] == b'+' || bytes[exponent] == b'-') {
            exponent += 1;
        }
        let exponent_digits = exponent;
        while exponent < bytes.len() && bytes[exponent].is_ascii_digit() {
            exponent += 1;
        }
        if exponent > exponent_digits {
            index = exponent;
        }
    }
    let value = js_parse_float(&text[..index])?;
    Some((value, &text[index..]))
}

/// Parses a number like JavaScript's `parseFloat` does: the longest valid prefix.
fn js_parse_float(text: &str) -> Option<f64> {
    if let Ok(value) = text.parse::<f64>() {
        return Some(value);
    }
    let bytes = text.as_bytes();
    let mut index = 0;
    if index < bytes.len() && (bytes[index] == b'+' || bytes[index] == b'-') {
        index += 1;
    }
    let mut digits = 0;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
        digits += 1;
    }
    if index < bytes.len() && bytes[index] == b'.' {
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    let mantissa = text[..index].parse::<f64>().ok()?;
    if index < bytes.len() && (bytes[index] == b'e' || bytes[index] == b'E') {
        let mut exponent = index + 1;
        let sign = if exponent < bytes.len() && (bytes[exponent] == b'+' || bytes[exponent] == b'-')
        {
            exponent += 1;
            -1
        } else {
            1
        };
        let exponent_start = exponent;
        while exponent < bytes.len() && bytes[exponent].is_ascii_digit() {
            exponent += 1;
        }
        if exponent > exponent_start {
            let power: i32 = text[index + 1..exponent].parse().ok()?;
            return Some(mantissa * 10f64.powi(sign * power));
        }
    }
    Some(mantissa)
}

/// Removes a prefix, ignoring the case, like the `i` flag of the upstream regular expressions.
fn strip_prefix_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let bytes = text.as_bytes();
    if bytes.len() >= prefix.len() && bytes[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
    {
        Some(&text[prefix.len()..])
    } else {
        None
    }
}

/// Skips the whitespace, but only if there is some, like the `\s+` of the upstream patterns.
fn skip_whitespace(text: &str) -> Option<&str> {
    let trimmed = text.trim_start();
    if trimmed.len() == text.len() {
        None
    } else {
        Some(trimmed)
    }
}

/// A color of the 256 color ANSI palette.
pub fn indexed_color(index: f64) -> Result<Color, String> {
    if !index.is_finite() || index.fract() != 0.0 || index < 0.0 || index > 255.0 {
        return Err(format!(
            "ANSI color index must be an integer from 0 to 255: {}",
            format_number(index)
        ));
    }
    Ok(Color::Indexed(index as u8))
}

/// A color given by its sRGB channels.
pub fn rgb_color(r: f64, g: f64, b: f64) -> Result<Color, String> {
    for (name, value) in [("r", r), ("g", g), ("b", b)] {
        if !value.is_finite() {
            return Err(format!("{name} must be finite"));
        }
        if !(0.0..=255.0).contains(&value) {
            return Err(format!(
                "{name} must be between 0 and 255: {}",
                format_number(value)
            ));
        }
    }
    Ok(Color::Rgb(RgbColorValue { r, g, b }))
}

/// A color given by its OKLCH channels.
pub fn oklch_color(l: f64, c: f64, h: f64) -> Result<Color, String> {
    if !l.is_finite() {
        return Err("l must be finite".to_string());
    }
    if !c.is_finite() {
        return Err("c must be finite".to_string());
    }
    if !h.is_finite() {
        return Err("h must be finite".to_string());
    }
    if !(0.0..=1.0).contains(&l) {
        return Err(format!("l must be between 0 and 1: {}", format_number(l)));
    }
    if c < 0.0 {
        return Err(format!("c must not be negative: {}", format_number(c)));
    }
    Ok(Color::Oklch(OklchColorValue {
        l,
        c,
        h: ((h % 360.0) + 360.0) % 360.0,
    }))
}

/// An OKHSL color, converted to sRGB. Saturation is relative to the sRGB gamut at the hue and lightness,
/// so equal saturation looks equally colorful across hues and lightness.
///
/// `h` is the hue in degrees, `s` and `l` are 0-1.
pub fn okhsl_color(h: f64, s: f64, l: f64) -> Result<Color, String> {
    if !h.is_finite() {
        return Err("h must be finite".to_string());
    }
    if !s.is_finite() {
        return Err("s must be finite".to_string());
    }
    if !l.is_finite() {
        return Err("l must be finite".to_string());
    }
    if !(0.0..=1.0).contains(&s) {
        return Err(format!("s must be between 0 and 1: {}", format_number(s)));
    }
    if !(0.0..=1.0).contains(&l) {
        return Err(format!("l must be between 0 and 1: {}", format_number(l)));
    }
    let rgb = oklab::okhsl_to_rgb(h, s, l);
    rgb_color(rgb.r, rgb.g, rgb.b)
}

/// The OKHSL channels of a color.
pub fn color_to_okhsl(color: &Color) -> OkhslChannels {
    oklab::rgb_to_okhsl(&color_to_rgb(color))
}

/// Parses an OKLCH color, like `oklch(62% 0.1 200)`, into its channels.
fn parse_oklch(value: &str) -> Option<(f64, f64, f64)> {
    let rest = strip_prefix_ci(value, "oklch(")?;
    let inner = rest.strip_suffix(')')?;
    let rest = inner.trim_start();
    let (lightness, rest) = parse_number(rest)?;
    let (rest, percent) = match rest.strip_prefix('%') {
        Some(rest) => (rest, true),
        None => (rest, false),
    };
    let rest = skip_whitespace(rest)?;
    let (chroma, rest) = parse_number(rest)?;
    let rest = skip_whitespace(rest)?;
    let (hue, rest) = parse_number(rest)?;
    let rest = strip_prefix_ci(rest, "deg").unwrap_or(rest);
    if !rest.trim_end().is_empty() {
        return None;
    }
    let lightness = if percent {
        lightness / 100.0
    } else {
        lightness
    };
    Some((lightness, chroma, hue))
}

/// Parses an OKHSL color, like `okhsl(250deg 60% 55%)`, into its channels.
fn parse_okhsl(value: &str) -> Option<(f64, f64, f64)> {
    let rest = strip_prefix_ci(value, "okhsl(")?;
    let inner = rest.strip_suffix(')')?;
    let rest = inner.trim_start();
    let (hue, rest) = parse_number(rest)?;
    let rest = strip_prefix_ci(rest, "deg").unwrap_or(rest);
    let rest = skip_whitespace(rest)?;
    let (saturation, rest) = parse_number(rest)?;
    let (rest, saturation_percent) = match rest.strip_prefix('%') {
        Some(rest) => (rest, true),
        None => (rest, false),
    };
    let rest = skip_whitespace(rest)?;
    let (lightness, rest) = parse_number(rest)?;
    let (rest, lightness_percent) = match rest.strip_prefix('%') {
        Some(rest) => (rest, true),
        None => (rest, false),
    };
    if !rest.trim_end().is_empty() {
        return None;
    }
    let saturation = if saturation_percent {
        saturation / 100.0
    } else {
        saturation
    };
    let lightness = if lightness_percent {
        lightness / 100.0
    } else {
        lightness
    };
    Some((hue, saturation, lightness))
}

/// Parses a color: a hex value, an OKLCH value or an OKHSL value.
pub fn parse_color(value: &str) -> Result<Color, String> {
    if let Some(hex) = value.strip_prefix('#')
        && (hex.len() == 3 || hex.len() == 6)
        && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        let digits = if hex.len() == 3 {
            hex.bytes()
                .flat_map(|byte| [byte, byte])
                .collect::<Vec<_>>()
        } else {
            hex.bytes().collect::<Vec<_>>()
        };
        let channel = |index: usize| -> u8 {
            let high = (digits[index * 2] as char).to_digit(16).unwrap_or(0);
            let low = (digits[index * 2 + 1] as char).to_digit(16).unwrap_or(0);
            ((high << 4) | low) as u8
        };
        return rgb_color(
            f64::from(channel(0)),
            f64::from(channel(1)),
            f64::from(channel(2)),
        );
    }
    if let Some((lightness, chroma, hue)) = parse_oklch(value) {
        return oklch_color(lightness, chroma, hue);
    }
    if let Some((hue, saturation, lightness)) = parse_okhsl(value) {
        return okhsl_color(hue, saturation, lightness);
    }
    Err(format!("Invalid color value: {value}"))
}

/// Parses a color given as text or as a palette index.
pub fn parse_color_value(input: ColorInput<'_>) -> Result<Color, String> {
    match input {
        ColorInput::Text(text) => parse_color(text),
        ColorInput::Index(index) => indexed_color(index),
    }
}

const BASIC_COLORS: [[f64; 3]; 16] = [
    [0.0, 0.0, 0.0],
    [128.0, 0.0, 0.0],
    [0.0, 128.0, 0.0],
    [128.0, 128.0, 0.0],
    [0.0, 0.0, 128.0],
    [128.0, 0.0, 128.0],
    [0.0, 128.0, 128.0],
    [192.0, 192.0, 192.0],
    [128.0, 128.0, 128.0],
    [255.0, 0.0, 0.0],
    [0.0, 255.0, 0.0],
    [255.0, 255.0, 0.0],
    [0.0, 0.0, 255.0],
    [255.0, 0.0, 255.0],
    [0.0, 255.0, 255.0],
    [255.0, 255.0, 255.0],
];
const CUBE_VALUES: [f64; 6] = [0.0, 95.0, 135.0, 175.0, 215.0, 255.0];
const GRAY_VALUES: [f64; 24] = [
    8.0, 18.0, 28.0, 38.0, 48.0, 58.0, 68.0, 78.0, 88.0, 98.0, 108.0, 118.0, 128.0, 138.0, 148.0,
    158.0, 168.0, 178.0, 188.0, 198.0, 208.0, 218.0, 228.0, 238.0,
];

/// The sRGB channels of a color of the 256 color ANSI palette.
fn indexed_to_rgb(index: u8) -> RgbColorValue {
    if index < 16 {
        let color = BASIC_COLORS[usize::from(index)];
        return RgbColorValue {
            r: color[0],
            g: color[1],
            b: color[2],
        };
    }
    if index < 232 {
        let cube = usize::from(index) - 16;
        return RgbColorValue {
            r: CUBE_VALUES[cube / 36],
            g: CUBE_VALUES[(cube % 36) / 6],
            b: CUBE_VALUES[cube % 6],
        };
    }
    let gray = (8 + (usize::from(index) - 232) * 10) as f64;
    RgbColorValue {
        r: gray,
        g: gray,
        b: gray,
    }
}

fn is_in_srgb_gamut(linear: [f64; 3]) -> bool {
    const EPSILON: f64 = 1e-7;
    linear
        .iter()
        .all(|channel| *channel >= -EPSILON && *channel <= 1.0 + EPSILON)
}

/// The sRGB channels of an OKLCH color, gamut mapped to the sRGB gamut.
fn oklch_to_rgb(color: &OklchColorValue) -> RgbColorValue {
    // Gamut mapping keeps the hue fixed, so its direction is computed once and scaled by chroma.
    let radians = (color.h * std::f64::consts::PI) / 180.0;
    let cos = radians.cos();
    let sin = radians.sin();
    let at_chroma =
        |chroma: f64| oklab::oklab_to_linear_srgb([color.l, chroma * cos, chroma * sin]);

    let direct = at_chroma(color.c);
    if is_in_srgb_gamut(direct) {
        return oklab::linear_srgb_to_rgb(direct);
    }

    // Reduce chroma until the color fits. The achromatic color is always in gamut, so it is the
    // fallback when no bisection step fits, e.g. `oklch(100% 0.3 150)` must map to white.
    let mut linear = at_chroma(0.0);
    let mut low = 0.0;
    let mut high = color.c;
    for _ in 0..20 {
        let chroma = (low + high) / 2.0;
        let candidate = at_chroma(chroma);
        if is_in_srgb_gamut(candidate) {
            low = chroma;
            linear = candidate;
        } else {
            high = chroma;
        }
    }
    oklab::linear_srgb_to_rgb(linear)
}

/// The sRGB channels of a color.
pub fn color_to_rgb(color: &Color) -> RgbColorValue {
    match color {
        Color::Indexed(index) => indexed_to_rgb(*index),
        Color::Rgb(rgb) => *rgb,
        Color::Oklch(oklch) => oklch_to_rgb(oklch),
    }
}

/// The OKLCH channels of a color.
pub fn color_to_oklch(color: &Color) -> OklchColorValue {
    if let Color::Oklch(oklch) = color {
        return *oklch;
    }
    let [l, a, b] = oklab::rgb_to_oklab(&color_to_rgb(color));
    OklchColorValue {
        l,
        c: oklab::chroma(a, b),
        h: ((libm::atan2(b, a) * 180.0) / std::f64::consts::PI + 360.0) % 360.0,
    }
}

/// The hex representation of a color, like `#ff0000`.
pub fn color_to_hex(color: &Color) -> String {
    let rgb = color_to_rgb(color);
    let channel = |value: f64| format!("{:02x}", js_round(value) as i64);
    format!("#{}{}{}", channel(rgb.r), channel(rgb.g), channel(rgb.b))
}

/// Mixes two colors, in the OKLCH space by default.
pub fn mix_colors(
    first: &Color,
    second: &Color,
    amount: f64,
    space: ColorMixSpace,
) -> Result<Color, String> {
    if !amount.is_finite() {
        return Err("amount must be finite".to_string());
    }
    if !(0.0..=1.0).contains(&amount) {
        return Err(format!(
            "amount must be between 0 and 1: {}",
            format_number(amount)
        ));
    }

    if space == ColorMixSpace::Srgb {
        let a = color_to_rgb(first);
        let b = color_to_rgb(second);
        return rgb_color(
            a.r + (b.r - a.r) * amount,
            a.g + (b.g - a.g) * amount,
            a.b + (b.b - a.b) * amount,
        );
    }

    let a = color_to_oklch(first);
    let b = color_to_oklch(second);
    let first_hue = if a.c < 1e-7 { b.h } else { a.h };
    let second_hue = if b.c < 1e-7 { first_hue } else { b.h };
    let hue_delta = ((second_hue - first_hue + 540.0) % 360.0) - 180.0;
    oklch_color(
        a.l + (b.l - a.l) * amount,
        a.c + (b.c - a.c) * amount,
        first_hue + hue_delta * amount,
    )
}

/// The index of the value closest to the target.
fn find_closest(values: &[f64], target: f64) -> usize {
    let mut closest_index = 0;
    let mut closest_distance = f64::INFINITY;
    for (index, value) in values.iter().enumerate() {
        let distance = (target - value).abs();
        if distance < closest_distance {
            closest_index = index;
            closest_distance = distance;
        }
    }
    closest_index
}

/// The squared distance of two colors, weighted like the luma.
fn color_distance(first: &RgbColorValue, second: &RgbColorValue) -> f64 {
    let dr = first.r - second.r;
    let dg = first.g - second.g;
    let db = first.b - second.b;
    dr * dr * 0.299 + dg * dg * 0.587 + db * db * 0.114
}

/// The 256 color palette index closest to a color.
fn rgb_to_ansi256(color: &RgbColorValue) -> u16 {
    let r_index = find_closest(&CUBE_VALUES, color.r);
    let g_index = find_closest(&CUBE_VALUES, color.g);
    let b_index = find_closest(&CUBE_VALUES, color.b);
    let cube_color = RgbColorValue {
        r: CUBE_VALUES[r_index],
        g: CUBE_VALUES[g_index],
        b: CUBE_VALUES[b_index],
    };
    let cube_index = 16 + 36 * r_index + 6 * g_index + b_index;

    let gray = js_round(0.299 * color.r + 0.587 * color.g + 0.114 * color.b);
    let gray_offset = find_closest(&GRAY_VALUES, gray);
    let gray_value = GRAY_VALUES[gray_offset];
    let spread = color.r.max(color.g).max(color.b) - color.r.min(color.g).min(color.b);
    let gray_color = RgbColorValue {
        r: gray_value,
        g: gray_value,
        b: gray_value,
    };
    if spread < 10.0 && color_distance(color, &gray_color) < color_distance(color, &cube_color) {
        return 232 + gray_offset as u16;
    }
    cube_index as u16
}

/// The escape sequence setting a color.
fn color_ansi(color: &Color, mode: TerminalColorMode, background: bool) -> String {
    let base = if background { 48 } else { 38 };
    if let Color::Indexed(index) = color {
        return format!("\x1b[{base};5;{index}m");
    }
    let rgb = color_to_rgb(color);
    if mode == TerminalColorMode::Truecolor {
        return format!(
            "\x1b[{base};2;{};{};{}m",
            js_round(rgb.r) as i64,
            js_round(rgb.g) as i64,
            js_round(rgb.b) as i64
        );
    }
    format!("\x1b[{base};5;{}m", rgb_to_ansi256(&rgb))
}

/// The escape sequence setting the foreground color.
pub fn foreground_ansi(color: &Color, mode: TerminalColorMode) -> String {
    color_ansi(color, mode, false)
}

/// The escape sequence setting the background color.
pub fn background_ansi(color: &Color, mode: TerminalColorMode) -> String {
    color_ansi(color, mode, true)
}

/// Styles a text with the colors and the attributes of the style.
pub fn style_text(text: &str, options: &TextStyle, mode: TerminalColorMode) -> String {
    let fg = options.fg.map(|color| foreground_ansi(&color, mode));
    let bg = options.bg.map(|color| background_ansi(&color, mode));
    style_text_with_ansi(text, fg.as_deref(), bg.as_deref(), &options.attributes)
}

/// Like [`style_text`], but with precomputed color escape sequences, e.g. cached theme colors.
/// The colors of `options` are ignored.
pub fn style_text_with_ansi(
    text: &str,
    fg_ansi: Option<&str>,
    bg_ansi: Option<&str>,
    options: &TextAttributes,
) -> String {
    // Resets are prepended so they close in reverse order of the opening sequences.
    let mut prefix = String::new();
    let mut suffix = String::new();
    if let Some(fg) = fg_ansi.filter(|ansi| !ansi.is_empty()) {
        prefix.push_str(fg);
        suffix.push_str("\x1b[39m");
    }
    if let Some(bg) = bg_ansi.filter(|ansi| !ansi.is_empty()) {
        prefix.push_str(bg);
        suffix = format!("\x1b[49m{suffix}");
    }
    if options.bold {
        prefix.push_str("\x1b[1m");
    }
    if options.dim {
        prefix.push_str("\x1b[2m");
    }
    if options.bold || options.dim {
        suffix = format!("\x1b[22m{suffix}");
    }
    if options.italic {
        prefix.push_str("\x1b[3m");
        suffix = format!("\x1b[23m{suffix}");
    }
    if options.underline {
        prefix.push_str("\x1b[4m");
        suffix = format!("\x1b[24m{suffix}");
    }
    if options.inverse {
        prefix.push_str("\x1b[7m");
        suffix = format!("\x1b[27m{suffix}");
    }
    if options.strikethrough {
        prefix.push_str("\x1b[9m");
        suffix = format!("\x1b[29m{suffix}");
    }
    format!("{prefix}{text}{suffix}")
}
