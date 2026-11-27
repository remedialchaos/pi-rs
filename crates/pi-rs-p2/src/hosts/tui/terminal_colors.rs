//! OSC 10/11/4 terminal color probing and mode 2031 light/dark scheme tracking
//! (ported from `apps/p2/src/hosts/tui-terminal-colors.ts`).

use pi_rs_p2_tui::theme::TerminalState;
use pi_rs_tui::RgbColor;
use std::env;
use std::time::{Duration, Instant};

const PALETTE_SIZE: usize = 16;

pub fn terminal_color_query() -> String {
    let mut query = String::from("\x1b]10;?\x07\x1b]11;?\x07");
    for index in 0..PALETTE_SIZE {
        query.push_str(&format!("\x1b]4;{index};?\x07"));
    }
    query.push_str("\x1b[c");
    query
}

fn channel(hex: &str) -> Option<f64> {
    if hex.is_empty() || hex.len() > 4 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let val = u32::from_str_radix(hex, 16).ok()?;
    let denom = ((1u64 << (4 * hex.len())) - 1) as f64;
    Some(((val as f64 / denom) * 255.0).round().clamp(0.0, 255.0))
}

/// Parse `#rrggbb`, `#rrrrggggbbbb`, or `rgb:r/g/b` (`rgba:r/g/b`).
pub fn parse_osc_color(raw: &str) -> Option<RgbColor> {
    let value = raw.trim();
    if let Some(hex) = value.strip_prefix('#') {
        if !matches!(hex.len(), 6 | 12) {
            return None;
        }
        let step = hex.len() / 3;
        return Some(RgbColor {
            r: channel(&hex[..step])?,
            g: channel(&hex[step..step * 2])?,
            b: channel(&hex[step * 2..step * 3])?,
        });
    }
    let stripped = value
        .strip_prefix("rgba:")
        .or_else(|| value.strip_prefix("RGBA:"))
        .or_else(|| value.strip_prefix("rgb:"))
        .or_else(|| value.strip_prefix("RGB:"))
        .unwrap_or(value);
    let mut parts = stripped.split('/');
    let r = channel(parts.next()?)?;
    let g = channel(parts.next()?)?;
    let b = channel(parts.next()?)?;
    Some(RgbColor { r, g, b })
}

/// Fallback dark/light detection from `$COLORFGBG` (`fg;bg`).
pub fn color_fg_bg_scheme() -> Option<String> {
    let raw = env::var("COLORFGBG").ok()?;
    let bg = raw.rsplit(';').next()?.trim();
    if bg.is_empty() || bg.len() > 2 || !bg.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let index: u8 = bg.parse().ok()?;
    if index > 15 {
        return None;
    }
    Some(if index <= 6 || index == 8 {
        "dark".to_string()
    } else {
        "light".to_string()
    })
}

fn scheme_from_bg(bg: RgbColor) -> String {
    let luminance = 0.2126 * bg.r + 0.7152 * bg.g + 0.0722 * bg.b;
    if luminance < 128.0 {
        "dark".to_string()
    } else {
        "light".to_string()
    }
}

struct PendingQuery {
    palette: [Option<RgbColor>; PALETTE_SIZE],
    replied: std::collections::HashSet<String>,
    deadline: Instant,
}

pub struct TerminalColorProbe {
    pub state: TerminalState,
    pending: Option<PendingQuery>,
    timeout: Duration,
    dirty: bool,
}

impl TerminalColorProbe {
    pub fn new(scheme: Option<String>, timeout_ms: u64) -> Self {
        Self {
            state: TerminalState {
                colors: Default::default(),
                scheme: scheme.or_else(color_fg_bg_scheme),
                pending: false,
            },
            pending: None,
            timeout: Duration::from_millis(timeout_ms),
            dirty: false,
        }
    }

    /// Returns escape sequence to enable mode 2031 reports and query OSC 10/11/4 + DA1.
    pub fn start_sequence(&mut self) -> String {
        let mut seq = String::from("\x1b[?2031h");
        seq.push_str(&self.begin_query());
        seq
    }

    pub fn stop_sequence(&mut self) -> &'static str {
        self.pending = None;
        self.state.pending = false;
        "\x1b[?2031l"
    }

    pub fn begin_query(&mut self) -> String {
        self.pending = Some(PendingQuery {
            palette: Default::default(),
            replied: Default::default(),
            deadline: Instant::now() + self.timeout,
        });
        self.state.pending = true;
        terminal_color_query()
    }

    /// Expires pending probe on timeout and returns updated state if anything changed.
    pub fn poll_timeout(&mut self) -> Option<TerminalState> {
        if let Some(pending) = &self.pending
            && Instant::now() >= pending.deadline
        {
            self.finish_query();
            return Some(self.state.clone());
        }
        if self.dirty {
            self.dirty = false;
            return Some(self.state.clone());
        }
        None
    }

    fn finish_query(&mut self) {
        let Some(query) = self.pending.take() else {
            return;
        };
        self.state.pending = false;
        if query.palette.iter().all(Option::is_some) {
            let mut palette = Vec::with_capacity(PALETTE_SIZE);
            for slot in query.palette.into_iter().flatten() {
                palette.push(slot);
            }
            self.state.colors.palette = Some(palette);
        }
        if self.state.scheme.is_none()
            && let Some(bg) = self.state.colors.background
        {
            self.state.scheme = Some(scheme_from_bg(bg));
        }
        self.dirty = true;
    }

    /// Consumes OSC 10/11/4, DA1, and mode 2031 replies from stdin chunks.
    /// Returns `(consumed_all, optional_follow_up_write)`.
    pub fn consume(&mut self, data: &str) -> (bool, Option<String>) {
        // Check mode 2031 scheme notification: `\x1b[?997;1n` (dark) or `\x1b[?997;2n` (light).
        if let Some(rest) = data.strip_prefix("\x1b[?997;")
            && let Some(code) = rest.strip_suffix('n')
            && matches!(code, "1" | "2")
        {
            self.state.scheme = Some(if code == "2" { "light" } else { "dark" }.to_string());
            let query = self.begin_query();
            return (true, Some(query));
        }

        // Check DA1: `\x1b[?...c`
        if self.pending.is_some()
            && data.starts_with("\x1b[?")
            && data.ends_with('c')
            && data[3..data.len() - 1]
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b';')
        {
            self.finish_query();
            return (true, None);
        }

        // Check OSC 10 / 11 / 4 color response: `\x1b](10|11|4;<idx>);<val>(\x07|\x1b\\)`
        if let Some(inner) = data
            .strip_prefix("\x1b]")
            .and_then(|s| s.strip_suffix('\x07').or_else(|| s.strip_suffix("\x1b\\")))
            && let Some((head, val)) = inner.split_once(';')
        {
            let rgb = parse_osc_color(val);
            if head == "10" {
                if let Some(c) = rgb {
                    self.state.colors.foreground = Some(c);
                    self.dirty = true;
                }
                self.record_reply("foreground");
                return (true, None);
            }
            if head == "11" {
                if let Some(c) = rgb {
                    self.state.colors.background = Some(c);
                    if self.state.scheme.is_none() {
                        self.state.scheme = Some(scheme_from_bg(c));
                    }
                    self.dirty = true;
                }
                self.record_reply("background");
                return (true, None);
            }
            if let Some(idx_str) = head.strip_prefix("4;")
                && let Ok(idx) = idx_str.parse::<usize>()
                && idx < PALETTE_SIZE
            {
                if let Some(c) = rgb
                    && let Some(pending) = self.pending.as_mut()
                {
                    pending.palette[idx] = Some(c);
                }
                self.record_reply(&idx.to_string());
                return (true, None);
            }
        }

        (false, None)
    }

    fn record_reply(&mut self, target: &str) {
        if let Some(pending) = self.pending.as_mut() {
            pending.replied.insert(target.to_string());
            if pending.replied.len() >= PALETTE_SIZE + 2 {
                self.finish_query();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_osc_hex_and_rgb_formats() {
        assert_eq!(
            parse_osc_color("#1e2030"),
            Some(RgbColor {
                r: 0x1e as f64,
                g: 0x20 as f64,
                b: 0x30 as f64
            })
        );
        assert_eq!(
            parse_osc_color("rgb:ffff/0000/8000"),
            Some(RgbColor {
                r: 255.0,
                g: 0.0,
                b: 128.0
            })
        );
        assert_eq!(parse_osc_color("not-a-color"), None);
    }

    #[test]
    fn probe_consumes_osc_colors_da1_and_mode_2031_scheme_updates() {
        let mut probe = TerminalColorProbe::new(None, 100);
        assert!(probe.start_sequence().contains("\x1b]10;?"));
        assert!(probe.consume("\x1b]10;#ffffff\x07").0);
        assert!(probe.consume("\x1b]11;rgb:f000/f000/f000\x1b\\").0);
        assert!(probe.consume("\x1b[?64;1c").0);
        assert_eq!(probe.state.scheme.as_deref(), Some("light"));
        let (consumed, follow_up) = probe.consume("\x1b[?997;1n");
        assert!(consumed);
        assert!(follow_up.is_some());
        assert_eq!(probe.state.scheme.as_deref(), Some("dark"));
        assert!(!probe.consume("hello").0);
    }
}
