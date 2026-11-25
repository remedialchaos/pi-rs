use std::rc::Rc;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::STANDARD};
use pi_rs_tui::terminal_image::hyperlink;
use pi_rs_tui::tui::Component;
use pi_rs_tui::{
    Color, ColorMixSpace, Image, ImageOptions, ImageTheme, RgbColorValue, TerminalColorMode, Text,
    background_ansi, foreground_ansi, get_keybindings, is_apple_terminal_session, mix_colors,
};

use crate::modes::interactive::theme::Theme;
use crate::utils::clipboard::{ClipboardEnv, Platform, copy_text};

mod armin;
pub use armin::ArminAnimation;
pub(super) use armin::{ARMIN_HEIGHT, ARMIN_WIDTH, is_armin_pixel};

const RESET: &str = "\x1b[0m";
pub(super) const CORAL: Color = rgb(228.0, 138.0, 122.0);
pub(super) const BLUE: Color = rgb(79.0, 142.0, 179.0);
pub(super) const YELLOW: Color = rgb(234.0, 182.0, 93.0);

const fn rgb(r: f64, g: f64, b: f64) -> Color {
    Color::Rgb(RgbColorValue { r, g, b })
}

pub fn pi_logo_lines(mode: TerminalColorMode) -> [String; 2] {
    let coral = foreground_ansi(&CORAL, mode);
    let blue_bg = background_ansi(&BLUE, mode);
    let blue = foreground_ansi(&BLUE, mode);
    let yellow = foreground_ansi(&YELLOW, mode);
    [
        format!("{coral}{blue_bg}▀{RESET}{coral}▀█{RESET} "),
        format!("{blue}█▀{RESET} {yellow}█{RESET}"),
    ]
}

pub fn supports_pi_logo() -> bool {
    !is_apple_terminal_session()
}

pub fn pi_wordmark(mode: TerminalColorMode) -> String {
    let coral = foreground_ansi(&CORAL, mode);
    let yellow = foreground_ansi(&YELLOW, mode);
    format!("{coral}P{RESET}{yellow}i{RESET}")
}

pub fn format_key_text(text: &str, capitalize: bool) -> String {
    format_key_text_for(text, capitalize, cfg!(target_os = "macos"))
}

fn format_key_text_for(text: &str, capitalize: bool, mac: bool) -> String {
    let mut result = String::with_capacity(text.len());
    for part in text.split_inclusive(['+', '/']) {
        let (part, separator) = part
            .as_bytes()
            .last()
            .and_then(|byte| {
                matches!(byte, b'+' | b'/').then(|| (&part[..part.len() - 1], Some(*byte as char)))
            })
            .unwrap_or((part, None));
        let part = if mac && part.eq_ignore_ascii_case("alt") {
            "option"
        } else {
            part
        };
        let mut chars = part.chars();
        if capitalize && let Some(first) = chars.next() {
            result.extend(first.to_uppercase());
            result.push_str(chars.as_str());
        } else {
            result.push_str(part);
        }
        if let Some(separator) = separator {
            result.push(separator);
        }
    }
    result
}

pub fn key_text(keybinding: &str) -> String {
    format_key_text(&get_keybindings().get_keys(keybinding).join("/"), false)
}

pub fn key_display_text(keybinding: &str) -> String {
    format_key_text(&get_keybindings().get_keys(keybinding).join("/"), true)
}

pub fn key_hint(theme: &Theme, keybinding: &str, description: &str) -> String {
    hint(theme, &key_text(keybinding), description)
}

pub fn raw_key_hint(theme: &Theme, key: &str, description: &str) -> String {
    hint(theme, &format_key_text(key, false), description)
}

fn hint(theme: &Theme, key: &str, description: &str) -> String {
    let mut result = theme.fg("dim", key);
    result.push_str(&theme.fg("muted", &format!(" {description}")));
    result
}

/// A wrapped hyperlink with platform-specific open and keybinding-aware clipboard hints.
pub struct AuthUrl {
    pub url: String,
    theme: Theme,
    copy_result: Option<Result<(), String>>,
}

impl AuthUrl {
    pub fn new(url: String, theme: Theme) -> Self {
        Self {
            url,
            theme,
            copy_result: None,
        }
    }

    /// The overlay host calls this for `app.message.copy` and wakes the renderer afterwards.
    pub async fn copy(&mut self) {
        let url = self.url.clone();
        self.copy_result = Some(
            tokio::task::spawn_blocking(move || {
                copy_text(&url, Platform::current(), &ClipboardEnv::from_env())
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result),
        );
    }
}

impl Component for AuthUrl {
    fn render(&self, width: usize) -> Vec<String> {
        let click = if cfg!(target_os = "macos") {
            "Cmd+click to open"
        } else {
            "Ctrl+click to open"
        };
        let suffix = match &self.copy_result {
            None => key_hint(&self.theme, "app.message.copy", "to copy"),
            Some(Ok(())) => self.theme.fg("success", "Copied URL to clipboard"),
            Some(Err(error)) => self.theme.fg("error", error),
        };
        let hint = format!(
            "{} {} {suffix}",
            self.theme.fg("dim", &hyperlink(click, &self.url)),
            self.theme.fg("dim", "•")
        );
        let url = self.theme.fg("accent", &hyperlink(&self.url, &self.url));
        let mut rows = Text::new(url, 1, 0, None).render(width);
        rows.extend(Text::new(hint, 1, 0, None).render(width));
        rows
    }
}

/// Apply a preferred partner's animated label to the selected row only.
pub fn provider_brand(
    provider: &str,
    text: &str,
    elapsed: Duration,
    mode: TerminalColorMode,
    light: bool,
) -> Option<String> {
    match provider {
        "radius" => {
            const BANDS: [Color; 4] = [
                rgb(77.0, 154.0, 191.0),
                rgb(131.0, 204.0, 210.0),
                rgb(241.0, 190.0, 87.0),
                rgb(240.0, 144.0, 130.0),
            ];
            let mut result = String::with_capacity(text.len() * 12 + 5);
            let offset = elapsed.as_secs_f64() * 10.0;
            for (index, ch) in text.chars().enumerate() {
                let position = (index as f64 - offset).rem_euclid(16.0) / 4.0;
                let band = position.floor() as usize % BANDS.len();
                let t = position.fract();
                let color = mix_colors(
                    &BANDS[band],
                    &BANDS[(band + 1) % BANDS.len()],
                    t * t * (3.0 - 2.0 * t),
                    ColorMixSpace::Srgb,
                )
                .unwrap_or(BANDS[band]);
                result.push_str(&foreground_ansi(&color, mode));
                result.push(ch);
            }
            result.push_str("\x1b[39m");
            Some(result)
        }
        "openrouter" => {
            let (base, peak) = if light {
                (rgb(93.0, 50.0, 225.0), rgb(143.0, 107.0, 255.0))
            } else {
                (rgb(198.0, 242.0, 67.0), rgb(239.0, 255.0, 176.0))
            };
            let amount = (1.0 - (std::f64::consts::TAU * elapsed.as_secs_f64() / 2.4).cos()) / 2.0;
            let color = mix_colors(&base, &peak, amount, ColorMixSpace::Oklch).unwrap_or(base);
            Some(format!("{}{text}\x1b[39m", foreground_ansi(&color, mode)))
        }
        _ => None,
    }
}

/// Poll this alongside the renderer. A new visible second (including zero) is delivered once.
/// The countdown is monotonic and does not drift if the event loop briefly stalls.
pub struct CountdownTimer {
    started: Instant,
    duration: Duration,
    last: Option<u64>,
    disposed: bool,
}

impl CountdownTimer {
    pub fn new(duration: Duration) -> Self {
        Self {
            started: Instant::now(),
            duration,
            last: None,
            disposed: false,
        }
    }

    pub fn remaining_seconds(&self, now: Instant) -> u64 {
        let remaining = self
            .duration
            .saturating_sub(now.saturating_duration_since(self.started));
        remaining
            .as_secs()
            .saturating_add(u64::from(remaining.subsec_nanos() > 0))
    }

    pub fn tick(&mut self, now: Instant) -> Option<u64> {
        if self.disposed {
            return None;
        }
        let seconds = self.remaining_seconds(now);
        if self.last == Some(seconds) {
            return None;
        }
        self.last = Some(seconds);
        Some(seconds)
    }

    pub fn is_expired(&self, now: Instant) -> bool {
        self.remaining_seconds(now) == 0
    }

    pub fn dispose(&mut self) {
        self.disposed = true;
    }
}

const BLOG_URL: &str = "https://mariozechner.at/posts/2026-04-08-ive-sold-out/";
const ANNOUNCEMENT_IMAGE: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/clankolas.png"));

pub struct EarendilAnnouncement {
    theme: Theme,
    image: Image,
}

impl EarendilAnnouncement {
    pub fn new(theme: Theme) -> Self {
        static BASE64: LazyLock<Arc<str>> =
            LazyLock::new(|| Arc::from(STANDARD.encode(ANNOUNCEMENT_IMAGE)));
        let image_theme = theme.clone();
        let image = Image::new(
            Arc::clone(&BASE64),
            "image/png".to_owned(),
            ImageTheme {
                fallback_color: Rc::new(move |text| image_theme.fg("muted", text)),
            },
            ImageOptions {
                max_width_cells: Some(56),
                filename: Some("clankolas.png".into()),
                ..ImageOptions::default()
            },
            None,
        );
        Self { theme, image }
    }
}

impl Component for EarendilAnnouncement {
    fn render(&self, width: usize) -> Vec<String> {
        if width == 0 {
            return Vec::new();
        }
        let border = self.theme.fg("accent", &"─".repeat(width));
        let heading = self
            .theme
            .fg("accent", "\x1b[1mpi has joined Earendil\x1b[22m");
        let label = self.theme.fg("muted", "Read the blog post:");
        let link = self.theme.fg("mdLink", &hyperlink(BLOG_URL, BLOG_URL));
        let mut lines = vec![border.clone()];
        lines.extend(Text::new(heading, 1, 0, None).render(width));
        lines.push(String::new());
        lines.extend(Text::new(label, 1, 0, None).render(width));
        lines.extend(Text::new(link, 1, 0, None).render(width));
        lines.push(String::new());
        lines.extend(self.image.render(width));
        lines.push(String::new());
        lines.push(border);
        lines
    }

    fn invalidate(&mut self) {
        self.image.invalidate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_rs_tui::{strip_terminal_sequences, visible_width};

    #[test]
    fn fixed_logo_and_partner_styles_preserve_visible_labels() {
        let mode = TerminalColorMode::Truecolor;
        assert_eq!(pi_logo_lines(mode).map(|line| visible_width(&line)), [4, 4]);
        assert_eq!(strip_terminal_sequences(&pi_wordmark(mode)), "Pi");
        let start = provider_brand("radius", "Radius", Duration::ZERO, mode, false);
        let later = provider_brand("radius", "Radius", Duration::from_millis(500), mode, false);
        assert_ne!(start, later);
        assert_eq!(
            later.as_deref().map(strip_terminal_sequences).as_deref(),
            Some("Radius")
        );
        assert!(provider_brand("faux", "Faux", Duration::ZERO, mode, false).is_none());
    }

    #[test]
    fn key_hints_preserve_alternatives_and_use_option_on_macos() {
        assert_eq!(
            format_key_text_for("alt+left/escape", true, true),
            "Option+Left/Escape"
        );
        assert_eq!(
            format_key_text_for("alt+left/escape", false, false),
            "alt+left/escape"
        );
    }

    #[test]
    fn countdown_ceil_skips_stale_frames_and_expires_once() {
        let mut timer = CountdownTimer::new(Duration::from_millis(2100));
        let start = timer.started;
        assert_eq!(timer.tick(start), Some(3));
        assert_eq!(timer.tick(start + Duration::from_millis(40)), None);
        assert_eq!(timer.tick(start + Duration::from_millis(2050)), Some(1));
        assert_eq!(timer.tick(start + Duration::from_millis(2100)), Some(0));
        assert!(timer.is_expired(start + Duration::from_secs(4)));
        assert_eq!(timer.tick(start + Duration::from_secs(4)), None);
    }
}
