//! The colors a terminal reports for its current theme.
//!
//! Ported from `packages/tui/src/terminal-colors.ts`.

use crate::colors::RgbColorValue;
use crate::oklab::js_round;

/// An RGB color, as the terminal reports it.
pub type RgbColor = RgbColorValue;

/// The color scheme of the terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalColorScheme {
    /// A dark theme.
    Dark,
    /// A light theme.
    Light,
}

/// The colors the terminal reports for its current theme.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TerminalColors {
    /// Default foreground (OSC 10).
    pub foreground: Option<RgbColor>,
    /// Default background (OSC 11).
    pub background: Option<RgbColor>,
    /// ANSI colors 0-15 (OSC 4). Only set when the terminal reported all 16.
    pub palette: Option<Vec<RgbColor>>,
}

/// What an OSC color reply reports: the default foreground (OSC 10), the default background (OSC 11),
/// or a palette index (OSC 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OscColorTarget {
    /// The default foreground (OSC 10).
    Foreground,
    /// The default background (OSC 11).
    Background,
    /// A palette index (OSC 4).
    Index(u16),
}

/// A parsed OSC color reply.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OscColorResponse {
    /// What the reply reports.
    pub target: OscColorTarget,
    /// The color, when the reply carries a parsable one.
    pub rgb: Option<RgbColor>,
}

/// The sRGB channels of a hex color, like `#ff0080`.
fn hex_to_rgb(hex: &str) -> Option<RgbColor> {
    let normalized = hex.strip_prefix('#').unwrap_or(hex);
    let channel = |start: usize| {
        u8::from_str_radix(&normalized[start..start + 2], 16)
            .ok()
            .map(f64::from)
    };
    Some(RgbColor {
        r: channel(0)?,
        g: channel(2)?,
        b: channel(4)?,
    })
}

/// The 0-255 value of a hex channel of any length, like `ffff` of a 12 digit color.
fn parse_osc_hex_channel(channel: &str) -> Option<f64> {
    if channel.is_empty() || !channel.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let max = 16f64.powi(channel.len() as i32) - 1.0;
    if max <= 0.0 {
        return None;
    }
    let value = match u64::from_str_radix(channel, 16) {
        Ok(value) => value as f64,
        Err(_) => {
            let mut value = 0f64;
            for byte in channel.bytes() {
                let digit = match byte {
                    b'0'..=b'9' => f64::from(byte - b'0'),
                    b'a'..=b'f' => f64::from(byte - b'a') + 10.0,
                    b'A'..=b'F' => f64::from(byte - b'A') + 10.0,
                    _ => return None,
                };
                value = value * 16.0 + digit;
            }
            value
        }
    };
    Some(js_round((value / max) * 255.0))
}

/// The color of an OSC color reply.
fn parse_osc_color_value(raw_value: &str) -> Option<RgbColor> {
    let value = raw_value.trim();
    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() == 6 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return hex_to_rgb(value);
        }
        if hex.len() == 12 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Some(RgbColor {
                r: parse_osc_hex_channel(&hex[0..4])?,
                g: parse_osc_hex_channel(&hex[4..8])?,
                b: parse_osc_hex_channel(&hex[8..12])?,
            });
        }
        return None;
    }

    let rgb_value = strip_prefix_ci(value, "rgba:")
        .or_else(|| strip_prefix_ci(value, "rgb:"))
        .unwrap_or(value);
    let mut parts = rgb_value.split('/');
    let red = parts.next()?;
    let green = parts.next()?;
    let blue = parts.next()?;
    Some(RgbColor {
        r: parse_osc_hex_channel(red)?,
        g: parse_osc_hex_channel(green)?,
        b: parse_osc_hex_channel(blue)?,
    })
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

/// Parses an OSC 10, 11, or 4 color reply. Returns `None` when `data` is not such a reply; the color is
/// `None` when the reply carries an unparseable color.
pub fn parse_osc_color_response(data: &str) -> Option<OscColorResponse> {
    let rest = data.strip_prefix("\x1b]")?;
    let (target, rest) = if let Some(rest) = rest.strip_prefix("10;") {
        (OscColorTarget::Foreground, rest)
    } else if let Some(rest) = rest.strip_prefix("11;") {
        (OscColorTarget::Background, rest)
    } else {
        let rest = rest.strip_prefix("4;")?;
        let digits_end = rest.find(';')?;
        let digits = &rest[..digits_end];
        if digits.is_empty()
            || digits.len() > 3
            || !digits.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        (
            OscColorTarget::Index(digits.parse::<u16>().ok()?),
            &rest[digits_end + 1..],
        )
    };

    let value_end = rest.find(['\x07', '\x1b']).unwrap_or(rest.len());
    let value = &rest[..value_end];
    let terminator = &rest[value_end..];
    if terminator != "\x07" && terminator != "\x1b\\" {
        return None;
    }
    Some(OscColorResponse {
        target,
        rgb: parse_osc_color_value(value),
    })
}

/// Parses the color scheme report of the terminal (the reply to `CSI ? 997 n`).
///
/// The report may arrive in several replies; the last one wins.
pub fn parse_terminal_color_scheme_report(data: &str) -> Option<TerminalColorScheme> {
    const PREFIX: &str = "\x1b[?997;";
    let mut rest = data;
    let mut scheme = None;
    while let Some(stripped) = rest.strip_prefix(PREFIX) {
        let mut characters = stripped.chars();
        let value = characters.next()?;
        if !matches!(value, '1' | '2') {
            return None;
        }
        if characters.next() != Some('n') {
            return None;
        }
        scheme = Some(if value == '2' {
            TerminalColorScheme::Light
        } else {
            TerminalColorScheme::Dark
        });
        rest = &stripped[value.len_utf8() + 1..];
    }
    if !rest.is_empty() {
        return None;
    }
    scheme
}
