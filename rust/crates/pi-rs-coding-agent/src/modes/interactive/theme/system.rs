use std::collections::HashMap;
use std::sync::LazyLock;

use pi_rs_tui::{
    Color, OkhslChannels, RgbColor, TerminalColorScheme, TerminalColors, color_to_okhsl,
    color_to_oklch, color_to_rgb, okhsl_color, oklab_to_okhsl_lightness, oklch_color,
};

use super::BACKGROUNDS;
use super::recipe::{Family, Level, NEUTRAL_FAMILY, RULES, families};

const BLACK: RgbColor = RgbColor {
    r: 0.0,
    g: 0.0,
    b: 0.0,
};
const WHITE: RgbColor = RgbColor {
    r: 255.0,
    g: 255.0,
    b: 255.0,
};
const BODY: [&str; 3] = ["text", "userMessageText", "toolTitle"];
const MIN_CONTRAST: f64 = 4.5;
const ITERATIONS: usize = 20;

type Colors = HashMap<&'static str, RgbColor>;

struct TokenRule {
    token: &'static str,
    family: Family,
    slot: u8,
    panel: bool,
    surfaces: Vec<(Level, &'static str)>,
}

static SOLVE_ORDER: LazyLock<Vec<TokenRule>> = LazyLock::new(|| {
    let families: HashMap<_, _> = families()
        .map(|(token, family, slot, _)| (token, (family, slot)))
        .collect();
    let mut rules: Vec<TokenRule> = Vec::with_capacity(families.len());
    for &(level, surfaces, group) in RULES {
        for &token in group {
            if let Some(prior) = rules.iter_mut().find(|rule| rule.token == token) {
                prior
                    .surfaces
                    .extend(surfaces.iter().map(|&surface| (level, surface)));
            } else if let Some(&(family, slot)) = families.get(token) {
                rules.push(TokenRule {
                    token,
                    family,
                    slot,
                    panel: BACKGROUNDS.contains(&token),
                    surfaces: surfaces.iter().map(|&surface| (level, surface)).collect(),
                });
            }
        }
    }
    rules
});

fn lightness(rgb: RgbColor) -> f64 {
    color_to_oklch(&Color::Rgb(rgb)).l
}

fn luminance(rgb: RgbColor) -> f64 {
    let linear = |channel: f64| {
        let v = channel / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(rgb.r) + 0.7152 * linear(rgb.g) + 0.0722 * linear(rgb.b)
}

fn contrast(first: RgbColor, second: RgbColor) -> f64 {
    let (a, b) = (luminance(first), luminance(second));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

pub(super) fn appearance(
    background: RgbColor,
    foreground: Option<RgbColor>,
) -> TerminalColorScheme {
    let white = contrast(WHITE, background);
    let black = contrast(BLACK, background);
    if let Some(foreground) = foreground {
        let distance = lightness(foreground) - lightness(background);
        if distance > 0.05 && white >= MIN_CONTRAST {
            return TerminalColorScheme::Dark;
        }
        if distance < -0.05 && black >= MIN_CONTRAST {
            return TerminalColorScheme::Light;
        }
    }
    if white >= black {
        TerminalColorScheme::Dark
    } else {
        TerminalColorScheme::Light
    }
}

fn from_okhsl(h: f64, s: f64, l: f64) -> RgbColor {
    // All recipe hues are finite; the solver bounds lightness and saturation.
    okhsl_color(h, s.clamp(0.0, 1.0), l.clamp(0.0, 1.0)).map_or(BLACK, |color| color_to_rgb(&color))
}

fn bell_weight(l: f64) -> f64 {
    let edge = (-2.0_f64).exp();
    ((-8.0 * (l - 0.5).powi(2)).exp() - edge) / (1.0 - edge)
}

#[derive(Clone, Copy)]
struct Source {
    hsl: OkhslChannels,
    chroma: f64,
}

impl Source {
    fn new(rgb: RgbColor) -> Self {
        let color = Color::Rgb(rgb);
        Self {
            hsl: color_to_okhsl(&color),
            chroma: color_to_oklch(&color).c,
        }
    }

    fn anchored(self, family: Family, l: f64, saturation: f64) -> RgbColor {
        let floor = family.min / family.max;
        let curve = |l| floor + (1.0 - floor) * bell_weight(l);
        let falloff = (curve(l) / curve(self.hsl.l)).min(1.0);
        let color = from_okhsl(self.hsl.h, self.hsl.s * falloff * saturation, l);
        let cap = self.chroma * falloff * saturation;
        let lch = color_to_oklch(&Color::Rgb(color));
        if lch.c <= cap {
            color
        } else {
            oklch_color(lch.l, cap, self.hsl.h).map_or(color, |color| color_to_rgb(&color))
        }
    }
}

fn move_until(close: f64, far: f64, mut acceptable: impl FnMut(f64) -> bool) -> (f64, f64) {
    let (mut close, mut far) = (close, far);
    for _ in 0..ITERATIONS {
        let middle = f64::midpoint(close, far);
        if acceptable(middle) {
            far = middle
        } else {
            close = middle
        }
    }
    (close, far)
}

struct Solver {
    background: RgbColor,
    background_l: f64,
    palette: Option<[Source; 16]>,
    saturation: f64,
    lighter: bool,
}

impl Solver {
    fn extreme(&self) -> f64 {
        if self.lighter { 1.0 } else { 0.0 }
    }

    fn strongest(&self, first: f64, second: f64) -> f64 {
        if self.lighter {
            first.max(second)
        } else {
            first.min(second)
        }
    }

    fn paint(&self, family: Family, slot: u8, lab_l: f64) -> RgbColor {
        let l = oklab_to_okhsl_lightness(lab_l);
        if let Some(palette) = &self.palette {
            palette[usize::from(slot)].anchored(family, l, self.saturation)
        } else {
            let saturation = family.min + (family.max - family.min) * bell_weight(l);
            from_okhsl(family.hue, saturation * self.saturation, l)
        }
    }

    fn target(&self, level: Level, surface_l: f64, relaxation: f64) -> Option<f64> {
        let reached = level.target(self.lighter, surface_l);
        let reached = if relaxation == 0.0 {
            reached?
        } else {
            reached.unwrap_or(self.extreme())
        };
        let floor_level = if self.lighter {
            Level::Readable
        } else {
            Level::Subtle
        };
        let floor = floor_level
            .target(self.lighter, surface_l)
            .unwrap_or(self.extreme())
            - surface_l;
        let distance = reached - surface_l;
        let compressed = if distance.abs() > floor.abs() {
            distance - (distance - floor) * relaxation.min(1.0)
        } else {
            distance
        };
        let target = surface_l + compressed * (1.0 - (relaxation - 1.0).max(0.0));
        (0.0..=1.0).contains(&target).then_some(target)
    }

    fn solve(&self, relaxation: f64, colors: &mut Colors) -> bool {
        colors.clear();
        // The draw surfaces precede their dependent tokens in the recipe.
        for rule in SOLVE_ORDER.iter() {
            let mut target = 1.0 - self.extreme();
            for &(level, surface) in &rule.surfaces {
                let surface_l = colors
                    .get(surface)
                    .copied()
                    .map_or(self.background_l, lightness);
                let Some(required) = self.target(level, surface_l, relaxation) else {
                    return false;
                };
                target = self.strongest(target, required);
            }
            let paint = |l| self.paint(rule.family, rule.slot, l);
            let mut rgb = paint(target);
            if rule.panel {
                let extreme = if self.lighter { WHITE } else { BLACK };
                if contrast(extreme, rgb) < MIN_CONTRAST {
                    let (_, readable) = move_until(target, self.background_l, |l| {
                        contrast(extreme, paint(l)) >= MIN_CONTRAST
                    });
                    rgb = paint(readable);
                }
            }
            colors.insert(rule.token, rgb);
        }
        true
    }

    fn body(
        &self,
        token: &str,
        colors: &Colors,
        foreground: Option<RgbColor>,
        relaxation: f64,
    ) -> Option<Color> {
        let surfaces = || {
            RULES
                .iter()
                .filter(move |(_, _, group)| group.contains(&token))
                .flat_map(|(_, surfaces, _)| {
                    surfaces
                        .iter()
                        .map(|surface| colors.get(surface).copied().unwrap_or(self.background))
                })
        };
        let mut text = colors.get(token).copied()?;
        if let Some(foreground) = foreground {
            let needed = surfaces().try_fold(1.0 - self.extreme(), |needed, surface| {
                self.target(Level::Emphasis, lightness(surface), relaxation)
                    .map(|target| self.strongest(needed, target))
            });
            if let Some(needed) = needed {
                let fg_l = lightness(foreground);
                if (self.lighter && fg_l >= needed || !self.lighter && fg_l <= needed)
                    && surfaces().all(|surface| contrast(foreground, surface) >= MIN_CONTRAST)
                {
                    return None;
                }
                text = Source::new(foreground).anchored(
                    NEUTRAL_FAMILY,
                    oklab_to_okhsl_lightness(needed),
                    self.saturation,
                );
            }
        }
        let meets = |rgb| surfaces().all(|surface| contrast(rgb, surface) >= MIN_CONTRAST);
        if !meets(text) {
            let channels = color_to_okhsl(&Color::Rgb(text));
            let at = |l| from_okhsl(channels.h, channels.s, l);
            let (_, readable) = move_until(channels.l, self.extreme(), |l| meets(at(l)));
            text = at(readable);
        }
        Some(Color::Rgb(text))
    }
}

fn solve(terminal: &TerminalColors, background: RgbColor) -> (Solver, Colors, f64) {
    let palette = terminal
        .palette
        .as_deref()
        .and_then(|palette| <&[RgbColor; 16]>::try_from(palette).ok());
    let solver = Solver {
        background,
        background_l: lightness(background),
        palette: palette.map(|palette| palette.map(Source::new)),
        saturation: 1.0,
        lighter: appearance(background, terminal.foreground) == TerminalColorScheme::Dark,
    };
    let mut colors = Colors::with_capacity(families().len());
    let mut relaxation = 0.0;
    if !solver.solve(relaxation, &mut colors) {
        let (_, solved) = move_until(0.0, 2.0, |t| solver.solve(t, &mut colors));
        relaxation = solved;
        solver.solve(relaxation, &mut colors);
    }
    (solver, colors, relaxation)
}

pub(super) fn generate(
    terminal: &TerminalColors,
    pending: bool,
) -> impl ExactSizeIterator<Item = (&'static str, Option<Color>, bool)> {
    let foreground = terminal.foreground;
    let solved = terminal
        .background
        .map(|background| solve(terminal, background));
    families().map(move |(token, _, slot, neutral)| {
        if let Some((solver, colors, relaxation)) = &solved {
            let color = if BODY.contains(&token) {
                solver.body(token, colors, foreground, *relaxation)
            } else {
                colors.get(token).copied().map(Color::Rgb)
            };
            (token, color, false)
        } else {
            let background = BACKGROUNDS.contains(&token);
            let color = (!background && !neutral && !pending).then_some(Color::Indexed(slot));
            let dim = !background && neutral && !BODY.contains(&token);
            (token, color, dim)
        }
    })
}
