use crate::theme::theme_tokens::{THEME_TOKENS, ThemeBg, ThemeColor, ThemeToken};
use pi_rs_tui::{
    Color, RgbColor, color_to_okhsl, color_to_oklch, color_to_rgb, okhsl_color,
    oklab_to_okhsl_lightness,
};
use std::collections::HashMap;
use std::sync::LazyLock;

pub const SYSTEM_THEME_NAME: &str = "system";

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Family {
    pub hue: f64,
    pub saturation: SaturationRange,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SaturationRange {
    pub min: f64,
    pub max: f64,
}

static FAMILIES: LazyLock<HashMap<&'static str, Family>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    m.insert(
        "neutral",
        Family {
            hue: 0.0,
            saturation: SaturationRange { min: 0.0, max: 0.0 },
        },
    );
    m.insert(
        "red",
        Family {
            hue: 20.0,
            saturation: SaturationRange {
                min: 0.25,
                max: 0.72,
            },
        },
    );
    m.insert(
        "yellow",
        Family {
            hue: 83.0,
            saturation: SaturationRange {
                min: 0.3,
                max: 0.88,
            },
        },
    );
    m.insert(
        "green",
        Family {
            hue: 159.0,
            saturation: SaturationRange {
                min: 0.3,
                max: 0.59,
            },
        },
    );
    m.insert(
        "blue",
        Family {
            hue: 232.0,
            saturation: SaturationRange {
                min: 0.3,
                max: 0.54,
            },
        },
    );
    m.insert(
        "violet",
        Family {
            hue: 295.0,
            saturation: SaturationRange { min: 0.3, max: 0.5 },
        },
    );
    m.insert(
        "cyan",
        Family {
            hue: 202.0,
            saturation: SaturationRange {
                min: 0.3,
                max: 0.58,
            },
        },
    );
    m
});

static TOKEN_FAMILIES: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    m.insert("accent", "violet");
    m.insert("border", "violet");
    m.insert("borderAccent", "violet");
    m.insert("borderMuted", "neutral");
    m.insert("success", "green");
    m.insert("error", "red");
    m.insert("warning", "yellow");
    m.insert("muted", "neutral");
    m.insert("dim", "neutral");
    m.insert("text", "neutral");
    m.insert("thinkingText", "violet");
    m.insert("scrollbarTrack", "neutral");
    m.insert("scrollbarThumb", "neutral");
    m.insert("selectedBg", "blue");
    m.insert("searchMatchBg", "yellow");
    m.insert("searchMatchText", "neutral");
    m.insert("userMessageBg", "blue");
    m.insert("userMessageText", "neutral");
    m.insert("customMessageBg", "violet");
    m.insert("customMessageText", "neutral");
    m.insert("customMessageLabel", "violet");
    m.insert("toolPendingBg", "neutral");
    m.insert("toolSuccessBg", "green");
    m.insert("toolErrorBg", "red");
    m.insert("toolTitle", "neutral");
    m.insert("toolOutput", "neutral");
    m.insert("mdHeading", "yellow");
    m.insert("mdLink", "blue");
    m.insert("mdLinkUrl", "neutral");
    m.insert("mdCode", "violet");
    m.insert("mdCodeBlock", "green");
    m.insert("mdCodeBlockBorder", "neutral");
    m.insert("mdQuote", "neutral");
    m.insert("mdQuoteBorder", "neutral");
    m.insert("mdHr", "neutral");
    m.insert("mdListBullet", "violet");
    m.insert("toolDiffAdded", "green");
    m.insert("toolDiffRemoved", "red");
    m.insert("toolDiffContext", "neutral");
    m.insert("syntaxComment", "neutral");
    m.insert("syntaxKeyword", "blue");
    m.insert("syntaxFunction", "yellow");
    m.insert("syntaxVariable", "cyan");
    m.insert("syntaxString", "yellow");
    m.insert("syntaxNumber", "green");
    m.insert("syntaxType", "violet");
    m.insert("syntaxOperator", "neutral");
    m.insert("syntaxPunctuation", "neutral");
    m.insert("thinkingOff", "neutral");
    m.insert("thinkingMinimal", "blue");
    m.insert("thinkingLow", "blue");
    m.insert("thinkingMedium", "violet");
    m.insert("thinkingHigh", "violet");
    m.insert("thinkingXhigh", "red");
    m.insert("thinkingMax", "red");
    m.insert("bashMode", "green");
    m
});

static TOKEN_SLOTS: LazyLock<HashMap<&'static str, usize>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    m.insert("syntaxString", 2);
    m.insert("syntaxNumber", 5);
    m.insert("searchMatchBg", 3);
    m
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThemeAppearance {
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Curve {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub reachable_min: f64,
    pub reachable_max: f64,
}

static LEVELS: LazyLock<HashMap<&'static str, HashMap<ThemeAppearance, Curve>>> =
    LazyLock::new(|| {
        let mut m = HashMap::new();
        let mut dark = HashMap::new();
        dark.insert(
            ThemeAppearance::Dark,
            Curve {
                a: 0.0,
                b: 0.0,
                c: 1.0,
                d: 0.0,
                reachable_min: 0.0,
                reachable_max: 1.0,
            },
        );
        dark.insert(
            ThemeAppearance::Light,
            Curve {
                a: 0.0,
                b: 0.0,
                c: 1.0,
                d: 0.0,
                reachable_min: 0.0,
                reachable_max: 1.0,
            },
        );
        m.insert("subtle", dark.clone());
        m.insert("readable", dark.clone());
        m.insert("emphasis", dark.clone());
        m.insert("strong", dark.clone());
        m
    });

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Level {
    Subtle,
    Readable,
    Emphasis,
    Strong,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Surface {
    ThemeBg(ThemeBg),
    Background,
    ScrollbarTrack,
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub token: ThemeToken,
    pub on: Vec<Surface>,
    pub level: Level,
}

static TOOL_PANELS: LazyLock<Vec<Surface>> = LazyLock::new(|| {
    vec![
        Surface::ThemeBg("toolPendingBg".to_string()),
        Surface::ThemeBg("toolSuccessBg".to_string()),
        Surface::ThemeBg("toolErrorBg".to_string()),
    ]
});

static PANELS: LazyLock<Vec<ThemeBg>> = LazyLock::new(|| {
    vec![
        "toolPendingBg".to_string(),
        "toolSuccessBg".to_string(),
        "toolErrorBg".to_string(),
        "userMessageBg".to_string(),
        "customMessageBg".to_string(),
    ]
});

static THINKING: LazyLock<Vec<ThemeColor>> = LazyLock::new(|| {
    vec![
        "thinkingOff".to_string(),
        "thinkingMinimal".to_string(),
        "thinkingLow".to_string(),
        "thinkingMedium".to_string(),
        "thinkingHigh".to_string(),
        "thinkingXhigh".to_string(),
        "thinkingMax".to_string(),
    ]
});

static THINKING_LEVELS: LazyLock<Vec<Level>> = LazyLock::new(|| {
    vec![
        Level::Subtle,
        Level::Readable,
        Level::Emphasis,
        Level::Strong,
        Level::Strong,
        Level::Strong,
        Level::Strong,
    ]
});

fn each(tokens: &[ThemeToken], on: &[Surface], level: Level) -> Vec<Rule> {
    tokens
        .iter()
        .cloned()
        .map(|token| Rule {
            token,
            on: on.to_vec(),
            level,
        })
        .collect()
}

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    let mut rules = Vec::new();
    rules.extend(each(
        &["accent".to_string()],
        &[Surface::Background],
        Level::Emphasis,
    ));
    rules.extend(each(
        &[
            "border".to_string(),
            "borderAccent".to_string(),
            "borderMuted".to_string(),
        ],
        &[Surface::Background],
        Level::Subtle,
    ));
    rules.extend(each(
        &[
            "success".to_string(),
            "error".to_string(),
            "warning".to_string(),
        ],
        &[Surface::Background],
        Level::Readable,
    ));
    rules.extend(each(
        &["muted".to_string(), "dim".to_string()],
        &[Surface::Background],
        Level::Subtle,
    ));
    rules.extend(each(
        &["text".to_string()],
        &[Surface::Background],
        Level::Emphasis,
    ));
    rules.extend(each(
        &["thinkingText".to_string()],
        &[Surface::Background],
        Level::Readable,
    ));
    rules.extend(each(
        &["scrollbarTrack".to_string()],
        &[Surface::Background],
        Level::Subtle,
    ));
    rules.extend(each(
        &["scrollbarThumb".to_string()],
        &[Surface::ScrollbarTrack],
        Level::Readable,
    ));
    rules.extend(each(
        &["selectedBg".to_string()],
        &[Surface::Background],
        Level::Subtle,
    ));
    rules.extend(each(
        &["searchMatchBg".to_string()],
        &[Surface::Background],
        Level::Subtle,
    ));
    rules.extend(each(
        &["searchMatchText".to_string()],
        &[Surface::ThemeBg("searchMatchBg".to_string())],
        Level::Readable,
    ));
    rules.extend(each(
        &["userMessageText".to_string()],
        &[Surface::ThemeBg("userMessageBg".to_string())],
        Level::Emphasis,
    ));
    rules.extend(each(
        &["customMessageText".to_string()],
        &[Surface::ThemeBg("customMessageBg".to_string())],
        Level::Readable,
    ));
    rules.extend(each(
        &["customMessageLabel".to_string()],
        &[Surface::ThemeBg("customMessageBg".to_string())],
        Level::Emphasis,
    ));
    rules.extend(each(
        &["toolTitle".to_string(), "toolOutput".to_string()],
        &TOOL_PANELS.iter().cloned().collect::<Vec<_>>(),
        Level::Readable,
    ));
    rules.extend(each(
        &[
            "mdHeading".to_string(),
            "mdLink".to_string(),
            "mdLinkUrl".to_string(),
            "mdCode".to_string(),
            "mdCodeBlock".to_string(),
            "mdCodeBlockBorder".to_string(),
            "mdQuote".to_string(),
            "mdQuoteBorder".to_string(),
            "mdHr".to_string(),
            "mdListBullet".to_string(),
        ],
        &[Surface::Background],
        Level::Readable,
    ));
    rules.extend(each(
        &[
            "toolDiffAdded".to_string(),
            "toolDiffRemoved".to_string(),
            "toolDiffContext".to_string(),
        ],
        &[Surface::Background],
        Level::Readable,
    ));
    rules.extend(each(
        &[
            "syntaxComment".to_string(),
            "syntaxKeyword".to_string(),
            "syntaxFunction".to_string(),
            "syntaxVariable".to_string(),
            "syntaxString".to_string(),
            "syntaxNumber".to_string(),
            "syntaxType".to_string(),
            "syntaxOperator".to_string(),
            "syntaxPunctuation".to_string(),
        ],
        &[Surface::Background],
        Level::Readable,
    ));
    for (i, token) in THINKING.iter().enumerate() {
        rules.extend(each(
            std::slice::from_ref(token),
            &[Surface::Background],
            THINKING_LEVELS[i],
        ));
    }
    rules.extend(each(
        &["bashMode".to_string()],
        &[Surface::Background],
        Level::Readable,
    ));
    rules
});

const FOREGROUND_LEVEL: Level = Level::Emphasis;

static FOREGROUND_TOKENS: LazyLock<Vec<ThemeColor>> = LazyLock::new(|| {
    vec![
        "text".to_string(),
        "userMessageText".to_string(),
        "toolTitle".to_string(),
    ]
});

const TEXT_MINIMUM_WCAG_CONTRAST: f64 = 4.5;

static SOLVE_ORDER: LazyLock<Vec<ThemeToken>> = LazyLock::new(|| {
    let mut order = Vec::new();
    order.push("background".to_string());
    order.push("scrollbarTrack".to_string());
    for panel in PANELS.iter() {
        order.push(panel.clone());
    }
    for token in THEME_TOKENS.keys() {
        if !order.contains(token) {
            order.push(token.clone());
        }
    }
    order
});

#[derive(Debug, Clone)]
pub struct SystemThemeInput {
    pub background: RgbColor,
    pub foreground: Option<RgbColor>,
    pub ansi: Option<Vec<RgbColor>>,
    pub color_mode: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SystemThemeColors {
    pub colors: HashMap<ThemeToken, String>,
    pub appearance: ThemeAppearance,
}

fn hex_of(color: RgbColor) -> String {
    let r = color.r.round() as u8;
    let g = color.g.round() as u8;
    let b = color.b.round() as u8;
    format!("#{:02x}{:02x}{:02x}", r, g, b)
}

fn bell_weight(lightness: f64) -> f64 {
    let sigma = 0.25;
    let center = 0.5;
    let diff = lightness - center;
    (-0.5 * (diff / sigma).powi(2)).exp()
}

fn saturation_curve(family: &Family, lightness: f64) -> f64 {
    let min = family.saturation.min;
    let max = family.saturation.max;
    if max == 0.0 {
        return 0.0;
    }
    min / max + (1.0 - min / max) * bell_weight(lightness)
}

fn level_target(level: Level, appearance: ThemeAppearance, surface_l: f64) -> Option<f64> {
    let levels = LEVELS.get(match level {
        Level::Subtle => "subtle",
        Level::Readable => "readable",
        Level::Emphasis => "emphasis",
        Level::Strong => "strong",
    })?;
    let curve = levels.get(&appearance)?;
    let target =
        curve.a * surface_l.powi(3) + curve.b * surface_l.powi(2) + curve.c * surface_l + curve.d;
    if surface_l < curve.reachable_min || surface_l > curve.reachable_max {
        return None;
    }
    Some(target)
}

fn okhsl_of(color: RgbColor) -> (f64, f64, f64) {
    let okhsl = color_to_okhsl(&Color::Rgb(color));
    (okhsl.h, okhsl.s, okhsl.l)
}

#[derive(Debug, Clone)]
struct SourceColor {
    h: f64,
    s: f64,
    l: f64,
    chroma: f64,
}

fn source_of(color: RgbColor) -> SourceColor {
    let (h, s, l) = okhsl_of(color);
    let chroma = color_to_oklch(&Color::Rgb(color)).c;
    SourceColor { h, s, l, chroma }
}

fn anchored(source: &SourceColor, family: &Family, lightness: f64, saturation: f64) -> RgbColor {
    let anchor = saturation_curve(family, source.l);
    let falloff = if anchor > 0.0 {
        (saturation_curve(family, lightness) / anchor).min(1.0)
    } else {
        1.0
    };
    let color = okhsl_color(source.h, source.s * falloff * saturation, lightness)
        .unwrap_or(Color::Indexed(0));
    let lch = color_to_oklch(&color);
    let cap = source.chroma * falloff * saturation;
    if lch.c > cap
        && let Ok(c) = pi_rs_tui::oklch_color(lch.l, cap, source.h)
    {
        return color_to_rgb(&c);
    }
    color_to_rgb(&color)
}

fn with_text_contrast(color: RgbColor, surfaces: &[RgbColor], lighter: bool) -> RgbColor {
    let mut current = color;
    for surface in surfaces {
        let contrast = wcag_contrast(current, *surface);
        if contrast < TEXT_MINIMUM_WCAG_CONTRAST {
            let target_l = if lighter {
                oklab_to_okhsl_lightness(color_to_oklch(&Color::Rgb(*surface)).l) + 0.1
            } else {
                oklab_to_okhsl_lightness(color_to_oklch(&Color::Rgb(*surface)).l) - 0.1
            };
            let (h, s, _) = okhsl_of(current);
            if let Ok(color) = okhsl_color(h, s, target_l.clamp(0.0, 1.0)) {
                current = color_to_rgb(&color);
            }
        }
    }
    current
}

fn relative_luminance(color: RgbColor) -> f64 {
    let to_linear = |channel: f64| {
        let c = channel / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * to_linear(color.r) + 0.7152 * to_linear(color.g) + 0.0722 * to_linear(color.b)
}

fn wcag_contrast(first: RgbColor, second: RgbColor) -> f64 {
    let l1 = relative_luminance(first);
    let l2 = relative_luminance(second);
    let lighter = l1.max(l2);
    let darker = l1.min(l2);
    (lighter + 0.05) / (darker + 0.05)
}

pub fn terminal_appearance(background: RgbColor, foreground: Option<RgbColor>) -> ThemeAppearance {
    let white = wcag_contrast(
        RgbColor {
            r: 255.,
            g: 255.,
            b: 255.,
        },
        background,
    );
    let black = wcag_contrast(
        RgbColor {
            r: 0.,
            g: 0.,
            b: 0.,
        },
        background,
    );
    if let Some(fg) = foreground {
        let delta = color_to_oklch(&Color::Rgb(fg)).l - color_to_oklch(&Color::Rgb(background)).l;
        if delta > 0.05 && white >= TEXT_MINIMUM_WCAG_CONTRAST {
            return ThemeAppearance::Dark;
        }
        if delta < -0.05 && black >= TEXT_MINIMUM_WCAG_CONTRAST {
            return ThemeAppearance::Light;
        }
    }
    if white >= black {
        ThemeAppearance::Dark
    } else {
        ThemeAppearance::Light
    }
}

pub fn generate_system_theme_colors(input: SystemThemeInput) -> SystemThemeColors {
    let appearance = terminal_appearance(input.background, input.foreground);
    let mut colors: HashMap<String, String> = HashMap::new();
    let bg_l = color_to_oklch(&Color::Rgb(input.background)).l;

    for token in SOLVE_ORDER.iter() {
        let family_name = TOKEN_FAMILIES
            .get(token.as_str())
            .copied()
            .unwrap_or("neutral");
        let family = FAMILIES
            .get(family_name)
            .copied()
            .unwrap_or(FAMILIES["neutral"]);
        let slot = TOKEN_SLOTS.get(token.as_str()).copied().unwrap_or(0);

        let rule = RULES.iter().find(|r| r.token == *token);
        if let Some(rule) = rule {
            for surface in &rule.on {
                let surface_l = match surface {
                    Surface::Background => bg_l,
                    Surface::ScrollbarTrack => bg_l,
                    Surface::ThemeBg(bg_token) => {
                        if let Some(existing) = colors.get(bg_token) {
                            if let Ok(parsed) = parse_color(existing) {
                                color_to_oklch(&Color::Rgb(parsed)).l
                            } else {
                                bg_l
                            }
                        } else {
                            bg_l
                        }
                    }
                };
                if let Some(target_l) = level_target(rule.level, appearance, surface_l) {
                    let source = if let Some(ansi) = &input.ansi {
                        if slot < ansi.len() {
                            source_of(ansi[slot])
                        } else {
                            source_of(input.background)
                        }
                    } else {
                        source_of(input.background)
                    };
                    let saturation = if family.saturation.max > 0.0 {
                        1.0
                    } else {
                        0.0
                    };
                    let color = anchored(&source, &family, target_l, saturation);
                    let mut surfaces_for_contrast = vec![input.background];
                    if FOREGROUND_TOKENS.contains(token) && rule.level == FOREGROUND_LEVEL {
                        surfaces_for_contrast.extend(PANELS.iter().map(|t| {
                            if let Some(existing) = colors.get(t) {
                                parse_color(existing).unwrap_or(input.background)
                            } else {
                                input.background
                            }
                        }));
                    }
                    let final_color =
                        if FOREGROUND_TOKENS.contains(token) && rule.level == FOREGROUND_LEVEL {
                            with_text_contrast(color, &surfaces_for_contrast, true)
                        } else {
                            color
                        };
                    colors.insert(token.clone(), hex_of(final_color));
                }
            }
        }
    }

    SystemThemeColors { colors, appearance }
}

fn parse_color(hex: &str) -> Result<RgbColor, String> {
    pi_rs_tui::parse_color(hex).map(|c| color_to_rgb(&c))
}

pub fn indexed_colors(saturation: f64, appearance: Option<ThemeAppearance>) -> SystemThemeColors {
    let mut colors = HashMap::new();
    for (&token, &family) in TOKEN_FAMILIES.iter() {
        let slot = match family {
            "red" => 1,
            "green" => 2,
            "yellow" => 3,
            "blue" => 4,
            "magenta" => 5,
            "cyan" => 6,
            _ => 0,
        };
        let color = if family == "neutral" || saturation <= 0.0 || PANELS.iter().any(|p| p == token)
        {
            String::new()
        } else {
            TOKEN_SLOTS.get(token).copied().unwrap_or(slot).to_string()
        };
        colors.insert(token.into(), color);
    }
    SystemThemeColors {
        colors,
        appearance: appearance.unwrap_or(ThemeAppearance::Dark),
    }
}
