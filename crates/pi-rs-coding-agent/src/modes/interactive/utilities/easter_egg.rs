use std::cell::RefCell;
use std::collections::HashMap;
use std::f64::consts::{PI, TAU};
use std::ops::Range;
use std::time::{Duration, Instant};

use pi_rs_tui::tui::{Component, TuiMouseEvent, TuiMouseEventResult, TuiMouseEventType};
use pi_rs_tui::utils::{grapheme_width, truncate_to_width, visible_width};
use pi_rs_tui::{
    Color, RgbColorValue, TerminalColorMode, TerminalColorScheme, TerminalColors, background_ansi,
    color_to_rgb, foreground_ansi, get_keybindings,
};
use unicode_normalization::char::is_combining_mark;

use super::super::theme::Theme;
use super::presentation::{
    ARMIN_HEIGHT, ARMIN_WIDTH, BLUE, CORAL, YELLOW, format_key_text, is_armin_pixel,
};

const FRAME: Duration = Duration::from_millis(33);
const FLY_START: f64 = 0.1;
const FLY_DURATION: f64 = 1.3;
const DUST_END: f64 = 1.12;
const EXIT_DURATION: f64 = 1.1;
const HALF_DEPTH: f64 = 0.35;
const PUZZLE_START: f64 = 4.4;
const PUZZLE_STEP: f64 = 0.3;
const PUZZLE_STEPS: usize = 12;
const SHUFFLE_LEN: f64 = PUZZLE_STEP * PUZZLE_STEPS as f64;
const PUZZLE_CYCLE: f64 = SHUFFLE_LEN + 1.6;
const DOT_BITS: [u8; 8] = [0x01, 0x08, 0x02, 0x10, 0x04, 0x20, 0x40, 0x80];
const MOVES: [Grid; 10] = [
    [1, 0, 0],
    [-1, 0, 0],
    [0, 1, 0],
    [0, -1, 0],
    [1, 0, 0],
    [-1, 0, 0],
    [0, 1, 0],
    [0, -1, 0],
    [0, 0, 1],
    [0, 0, -1],
];
const STAR_COLORS: [Rgb; 4] = [
    [255.0, 255.0, 255.0],
    [170.0, 195.0, 255.0],
    [255.0, 225.0, 170.0],
    [255.0, 190.0, 205.0],
];

type Rgb = [f64; 3];
type Vec3 = [f64; 3];
type Grid = [i32; 3];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EasterEgg3d {
    PiLogo { column: usize, row: usize },
    Armin,
}

struct Block {
    home: Grid,
    rgb: Rgb,
}

struct Model {
    columns: i32,
    rows: i32,
    blocks: Vec<Block>,
    camera: f64,
    width_share: f64,
    origin: Option<(usize, usize)>,
}

impl Model {
    fn new(egg: EasterEgg3d, accent: Rgb) -> Self {
        fn logo_pixel(x: usize, y: usize) -> Option<Rgb> {
            let color = match [b"ccc.", b"b.c.", b"bb.y", b"b..y"][y][x] {
                b'c' => CORAL,
                b'b' => BLUE,
                b'y' => YELLOW,
                _ => return None,
            };
            Some(rgb(color_to_rgb(&color)))
        }
        let (columns, rows, camera, width_share, origin) = match egg {
            EasterEgg3d::PiLogo { column, row } => (4, 4, 10.0, 0.35, Some((column, row))),
            EasterEgg3d::Armin => (ARMIN_WIDTH, ARMIN_HEIGHT, 80.0, 0.45, None),
        };
        let mut blocks = Vec::new();
        for y in 0..rows {
            for x in 0..columns {
                let color = if origin.is_some() {
                    logo_pixel(x, y)
                } else {
                    is_armin_pixel(x, y).then_some(accent)
                };
                if let Some(rgb) = color {
                    blocks.push(Block {
                        home: [x as i32, y as i32, 0],
                        rgb,
                    });
                }
            }
        }
        Self {
            columns: columns as i32,
            rows: rows as i32,
            blocks,
            camera,
            width_share,
            origin,
        }
    }

    fn pose(&self, width: usize, height: usize, progress: f64, time: f64) -> Pose {
        let phase = spin(time);
        let columns = f64::from(self.columns);
        let rows = f64::from(self.rows);
        let radius = (columns / 2.0).hypot(rows / 2.0).hypot(1.0 + HALF_DEPTH);
        let reach = radius * self.camera / (self.camera - radius);
        let start_scale = if self.origin.is_some() {
            2.0 * (self.camera - HALF_DEPTH) / self.camera
        } else {
            0.1
        };
        let end_x = width as f64;
        let end_y = height as f64 * 2.0 - 2.0;
        let (start_x, start_y) = self.origin.map_or((end_x, end_y), |(x, y)| {
            (x as f64 * 2.0 + columns, y as f64 * 4.0 + rows)
        });
        let end_scale = start_scale
            .max((end_x * 2.0 * self.width_share).min((height as f64 * 4.0 - 8.0) * 0.48) / reach);
        Pose {
            x: lerp(start_x, end_x, progress),
            y: lerp(start_y, end_y, progress),
            scale: start_scale * (end_scale / start_scale).powf(progress),
            yaw: phase - 0.6 * phase.sin(),
            pitch: 0.3 * phase.sin() * progress,
            roll: 0.06 * (2.0 * phase).sin() * progress,
        }
    }

    fn next_shuffle(&self, cycle: usize) -> Shuffle {
        let count = self.blocks.len();
        let mut positions = self
            .blocks
            .iter()
            .map(|block| block.home)
            .collect::<Vec<_>>();
        let mut steps = Vec::with_capacity(count * (PUZZLE_STEPS + 1));
        steps.extend_from_slice(&positions);
        let mut occupied = vec![false; (self.columns * self.rows * 3) as usize];
        let mut previous = vec![false; count];
        let mut moved = vec![false; count];
        let mut pools = [Vec::new(), Vec::new()];
        let mut random = (cycle as u32).wrapping_mul(7919).wrapping_add(1);
        let mut next = || {
            let value = hash(random);
            random = random.wrapping_add(1);
            value
        };
        let key = |[x, y, z]: Grid| (((z + 1) * self.rows + y) * self.columns + x) as usize;
        for _ in 0..PUZZLE_STEPS {
            moved.fill(false);
            occupied.fill(false);
            for &position in &positions {
                occupied[key(position)] = true;
            }
            let move_count = if self.origin.is_some() {
                if next() < 0.4 { 2 } else { 1 }
            } else {
                (count as f64 * (0.03 + next() * 0.04)).round() as usize
            };
            for _ in 0..move_count {
                pools.iter_mut().for_each(Vec::clear);
                for (block, [x, y, z]) in positions.iter().copied().enumerate() {
                    if moved[block] {
                        continue;
                    }
                    for [dx, dy, dz] in MOVES {
                        let target = [x + dx, y + dy, z + dz];
                        if !(0..self.columns).contains(&target[0])
                            || !(0..self.rows).contains(&target[1])
                            || !(-1..=1).contains(&target[2])
                            || occupied[key(target)]
                        {
                            continue;
                        }
                        pools[usize::from(previous[block])].push((block, target));
                    }
                }
                let [fresh, recent] = &pools;
                let pool = if fresh.is_empty() { recent } else { fresh };
                let Some(&(block, target)) = pool.get((next() * pool.len() as f64) as usize) else {
                    break;
                };
                occupied[key(target)] = true;
                moved[block] = true;
                positions[block] = target;
            }
            std::mem::swap(&mut previous, &mut moved);
            steps.extend_from_slice(&positions);
        }
        Shuffle { cycle, steps }
    }
}

struct Shuffle {
    cycle: usize,
    steps: Vec<Grid>,
}

impl Shuffle {
    fn offset(&self, index: usize, home: Grid, count: usize, time: f64) -> Vec3 {
        let time = (time - PUZZLE_START).rem_euclid(PUZZLE_CYCLE);
        if time < SHUFFLE_LEN {
            let step = ((time / PUZZLE_STEP).floor() as usize).min(PUZZLE_STEPS - 1);
            let p = smooth((time - step as f64 * PUZZLE_STEP) / PUZZLE_STEP);
            let from = self.steps[step * count + index];
            let to = self.steps[(step + 1) * count + index];
            return std::array::from_fn(|axis| {
                lerp(f64::from(from[axis]), f64::from(to[axis]), p) - f64::from(home[axis])
            });
        }
        if time < SHUFFLE_LEN + 1.0 {
            let p = time - SHUFFLE_LEN;
            let from = self.steps[PUZZLE_STEPS * count + index];
            let mut offset =
                std::array::from_fn(|axis| f64::from(from[axis] - home[axis]) * (1.0 - smooth(p)));
            offset[2] += (PI * p).sin() * if index.is_multiple_of(2) { 0.8 } else { -0.8 };
            offset
        } else {
            [0.0; 3]
        }
    }
}

struct Pose {
    x: f64,
    y: f64,
    scale: f64,
    yaw: f64,
    pitch: f64,
    roll: f64,
}

impl Pose {
    fn rotation(&self) -> [Vec3; 3] {
        let (sy, cy) = self.yaw.sin_cos();
        let (sx, cx) = self.pitch.sin_cos();
        let (sz, cz) = self.roll.sin_cos();
        [
            [cz * cy - sz * sx * sy, -sz * cx, cz * sy + sz * sx * cy],
            [sz * cy + cz * sx * sy, cz * cx, sz * sy - cz * sx * cy],
            [-cx * sy, sx, cx * cy],
        ]
    }
}

fn spin(time: f64) -> f64 {
    let base = |time: f64| {
        let elapsed = (time - FLY_START).max(0.0);
        let u = elapsed / 1.4;
        TAU / PUZZLE_CYCLE
            * if u < 1.0 {
                1.4 * (u.powi(6) - 3.0 * u.powi(5) + 2.5 * u.powi(4))
            } else {
                elapsed - 0.7
            }
    };
    let alignment = TAU - base(PUZZLE_START + SHUFFLE_LEN + 1.3).rem_euclid(TAU);
    base(time) + alignment * fly(time)
}

fn fly(time: f64) -> f64 {
    smooth((time - FLY_START) / FLY_DURATION)
}
fn smooth(value: f64) -> f64 {
    let t = value.clamp(0.0, 1.0);
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}
fn lerp(a: f64, b: f64, value: f64) -> f64 {
    a + (b - a) * value
}
fn mix(a: Rgb, b: Rgb, value: f64) -> Rgb {
    std::array::from_fn(|i| lerp(a[i], b[i], value))
}
fn rgb(color: RgbColorValue) -> Rgb {
    [color.r, color.g, color.b]
}
fn dot(a: Vec3, b: Vec3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn hash(value: u32) -> f64 {
    let mut x = (value ^ 0x9e37_79b9).wrapping_mul(0x85eb_ca6b);
    x ^= x >> 13;
    x = x.wrapping_mul(0xc2b2_ae35);
    x ^= x >> 16;
    f64::from(x) / 4_294_967_296.0
}
fn braille(bits: u8) -> char {
    char::from_u32(0x2800 + u32::from(bits)).unwrap_or(' ')
}

#[derive(Clone, Copy)]
struct Hit {
    depth: f64,
    color: Rgb,
}

impl Default for Hit {
    fn default() -> Self {
        Self {
            depth: f64::INFINITY,
            color: [0.0; 3],
        }
    }
}

#[derive(Default, Clone, Copy)]
struct Lit {
    bits: u8,
    count: u8,
    sum: Rgb,
}

impl Lit {
    fn quantize(self) -> Rgb {
        self.sum
            .map(|sum| (sum / (5.0 * f64::from(self.count))).round() * 5.0)
    }
}

#[derive(Default)]
struct Raster {
    dot_columns: usize,
    dot_rows: usize,
    dots: Vec<Hit>,
    cells: Vec<Lit>,
    /// Bounding box of projected faces, in braille dots.
    dirty: Option<[usize; 4]>,
}

impl Raster {
    fn clear(&mut self, width: usize, height: usize) {
        let (dot_columns, dot_rows) = (width * 2, height * 4);
        if (self.dot_columns, self.dot_rows) != (dot_columns, dot_rows) {
            self.dot_columns = dot_columns;
            self.dot_rows = dot_rows;
            self.dots.resize(dot_columns * dot_rows, Hit::default());
            self.dots.fill(Hit::default());
            self.cells.resize(width * height, Lit::default());
            self.cells.fill(Lit::default());
            self.dirty = None;
        } else if let Some([left, top, right, bottom]) = self.dirty.take() {
            for row in top..=bottom {
                let start = row * dot_columns;
                self.dots[start + left..=start + right].fill(Hit::default());
            }
            for row in top / 4..=bottom / 4 {
                let start = row * width;
                self.cells[start + left / 2..=start + right / 2].fill(Lit::default());
            }
        }
    }

    fn block(
        &mut self,
        model: &Model,
        pose: &Pose,
        matrix: &[Vec3; 3],
        position: Vec3,
        color: Rgb,
        light_background: bool,
    ) {
        let low = [
            position[0] - f64::from(model.columns) / 2.0,
            position[1] - f64::from(model.rows) / 2.0,
            position[2] - HALF_DEPTH,
        ];
        let high = [low[0] + 1.0, low[1] + 1.0, position[2] + HALF_DEPTH];
        let origin = matrix[2].map(|v| v * model.camera);
        for axis in 0..3 {
            let u = (axis + 1) % 3;
            let v = (axis + 2) % 3;
            let (plane, side) = if origin[axis] > high[axis] {
                (high[axis], 1.0)
            } else if origin[axis] < low[axis] {
                (low[axis], -1.0)
            } else {
                continue;
            };
            let normal = matrix.map(|basis| basis[axis] * side);
            let light = dot(normal, [-0.43, -0.57, 0.70]).max(0.0);
            let rim = (normal[0] * 0.8 - normal[2] * 0.3).max(0.0);
            let (light, specular) = if light_background {
                ((0.55 + light * 0.45 + rim * 0.1).min(1.0), 0.0)
            } else {
                (
                    0.45 + light * 0.78 + rim * 0.25,
                    153.0 * dot(normal, [-0.23, -0.30, 0.92]).max(0.0).powi(24),
                )
            };
            let shaded = color.map(|channel| (channel * light + specular).min(255.0));
            let mut left = f64::INFINITY;
            let mut top = f64::INFINITY;
            let mut right = f64::NEG_INFINITY;
            let mut bottom = f64::NEG_INFINITY;
            for pu in [low[u], high[u]] {
                for pv in [low[v], high[v]] {
                    let mut point = [0.0; 3];
                    point[axis] = plane;
                    point[u] = pu;
                    point[v] = pv;
                    let perspective =
                        pose.scale * model.camera / (model.camera - dot(matrix[2], point));
                    let x = pose.x + dot(matrix[0], point) * perspective;
                    let y = pose.y + dot(matrix[1], point) * perspective;
                    left = left.min(x);
                    top = top.min(y);
                    right = right.max(x);
                    bottom = bottom.max(y);
                }
            }
            if right < 0.0
                || bottom < 0.0
                || left >= self.dot_columns as f64
                || top >= self.dot_rows as f64
            {
                continue;
            }
            let (x0, x1) = (
                left.floor() as usize,
                (right.ceil() as usize).min(self.dot_columns - 1),
            );
            let (y0, y1) = (
                top.floor() as usize,
                (bottom.ceil() as usize).min(self.dot_rows - 1),
            );
            if let Some([left, top, right, bottom]) = &mut self.dirty {
                *left = (*left).min(x0);
                *top = (*top).min(y0);
                *right = (*right).max(x1);
                *bottom = (*bottom).max(y1);
            } else {
                self.dirty = Some([x0, y0, x1, y1]);
            }
            let step = matrix[0].map(|value| value / pose.scale);
            let sx = (x0 as f64 + 0.5 - pose.x) / pose.scale;
            for y in y0..=y1 {
                let sy = (y as f64 + 0.5 - pose.y) / pose.scale;
                let mut direction: Vec3 =
                    std::array::from_fn(|i| matrix[0][i] * sx + matrix[1][i] * sy - origin[i]);
                for x in x0..=x1 {
                    let distance = (plane - origin[axis]) / direction[axis];
                    let hit = &mut self.dots[y * self.dot_columns + x];
                    if distance > 0.0 && distance < hit.depth {
                        let pu = origin[u] + distance * direction[u];
                        let pv = origin[v] + distance * direction[v];
                        if (low[u]..=high[u]).contains(&pu) && (low[v]..=high[v]).contains(&pv) {
                            *hit = Hit {
                                depth: distance,
                                color: shaded,
                            };
                        }
                    }
                    for i in 0..3 {
                        direction[i] += step[i];
                    }
                }
            }
        }
    }

    fn pack(&mut self, width: usize, camera: f64, background: Rgb, light: bool) {
        let Some([left, top, right, bottom]) = self.dirty else {
            return;
        };
        for row in top / 4..=bottom / 4 {
            for col in left / 2..=right / 2 {
                let mut cell = Lit::default();
                for (ordinal, bit) in DOT_BITS.into_iter().enumerate() {
                    let hit = self.dots
                        [(row * 4 + ordinal / 2) * self.dot_columns + col * 2 + ordinal % 2];
                    if !hit.depth.is_finite() {
                        continue;
                    }
                    cell.bits |= bit;
                    cell.count += 1;
                    let fog = (0.15 - camera * (1.0 - hit.depth) * 0.12).clamp(0.0, 1.0)
                        * if light { 0.5 } else { 1.0 };
                    for (sum, sample) in cell.sum.iter_mut().zip(mix(hit.color, background, fog)) {
                        *sum += sample;
                    }
                }
                self.cells[row * width + col] = cell;
            }
        }
    }

    fn halo(
        &self,
        row: usize,
        col: usize,
        width: usize,
        height: usize,
        background: Rgb,
    ) -> Option<Rgb> {
        let [left, top, right, bottom] = self.dirty?;
        let (left, right) = (left / 2, right / 2);
        let (top, bottom) = (top / 4, bottom / 4);
        if col + 6 < left
            || col.saturating_sub(6) > right
            || row + 3 < top
            || row.saturating_sub(3) > bottom
        {
            return None;
        }
        let mut sum = [0.0; 3];
        let mut count = 0.0;
        for y in row.saturating_sub(3).max(top)..=(row + 3).min(height - 1).min(bottom) {
            let vertical = 4 - row.abs_diff(y);
            for x in col.saturating_sub(6).max(left)..=(col + 6).min(width - 1).min(right) {
                let cell = self.cells[y * width + x];
                let weight = (vertical * (7 - col.abs_diff(x))) as f64;
                count += f64::from(cell.count) * weight;
                for (sum, color) in sum.iter_mut().zip(cell.sum) {
                    *sum += color * weight;
                }
            }
        }
        let center = self.cells[row * width + col];
        let coverage = count / (8.0 * 784.0);
        let direct = f64::from(center.count) / 8.0;
        let amount = (0.3 * coverage.max(direct).min(1.0).powf(0.7) * 32.0).round() / 32.0;
        if amount <= 0.0 {
            return None;
        }
        let tint = if direct > coverage {
            center.quantize()
        } else {
            sum.map(|value| (value / (5.0 * count)).round() * 5.0)
        };
        Some(mix(background, tint, amount))
    }
}

struct ScreenCell {
    text: Range<usize>,
    width: usize,
    fg: Rgb,
    bg: Option<Rgb>,
    delay: f64,
    ink: u8,
}

#[derive(Default)]
struct ScreenStyle {
    foreground: Option<Rgb>,
    background: Option<Rgb>,
    dim: bool,
    inverse: bool,
}

fn palette(index: u8) -> Rgb {
    rgb(color_to_rgb(&Color::Indexed(index)))
}

impl ScreenStyle {
    fn sgr(&mut self, parameters: &str) {
        let mut codes = parameters.split(';');
        while let Some(value) = codes.next() {
            let mut parts = value.split(':');
            let code = parts.next().unwrap_or("");
            let code = if code.is_empty() {
                0
            } else if let Ok(code) = code.parse::<u8>() {
                code
            } else {
                continue;
            };
            match code {
                0 => *self = Self::default(),
                2 => self.dim = true,
                22 => self.dim = false,
                7 => self.inverse = true,
                27 => self.inverse = false,
                30..=37 => self.foreground = Some(palette(code - 30)),
                90..=97 => self.foreground = Some(palette(code - 90 + 8)),
                40..=47 => self.background = Some(palette(code - 40)),
                100..=107 => self.background = Some(palette(code - 100 + 8)),
                39 => self.foreground = None,
                49 => self.background = None,
                38 | 48 => {
                    let extended = |values: &mut dyn Iterator<Item = &str>, kind: &str| {
                        let mut channel = || values.next()?.parse::<u8>().ok();
                        match kind {
                            "5" => Some(palette(channel()?)),
                            "2" => Some([
                                f64::from(channel()?),
                                f64::from(channel()?),
                                f64::from(channel()?),
                            ]),
                            _ => None,
                        }
                    };
                    let color = if let Some(kind) = parts.next() {
                        let channels = parts.filter(|part| !part.is_empty()).collect::<Vec<_>>();
                        let channels = if kind == "2" && channels.len() == 4 {
                            &channels[1..]
                        } else {
                            &channels[..]
                        };
                        extended(&mut channels.iter().copied(), kind)
                    } else if let Some(kind) = codes.next() {
                        extended(&mut codes, kind)
                    } else {
                        None
                    };
                    if let Some(color) = color {
                        if code == 38 {
                            self.foreground = Some(color);
                        } else {
                            self.background = Some(color);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn parse_screen_line(
    line: &str,
    width: usize,
    foreground: Rgb,
    background: Rgb,
) -> Vec<ScreenCell> {
    let mut out = Vec::with_capacity(width.min(line.len()));
    let mut style = ScreenStyle::default();
    let mut pos = 0;
    while pos < line.len() && out.len() < width {
        let tail = &line[pos..];
        if let Some(escape) = tail.strip_prefix('\x1b') {
            match escape.as_bytes() {
                [b'[', rest @ ..] => {
                    let Some(end) = rest.iter().position(|byte| (0x40..=0x7e).contains(byte))
                    else {
                        break;
                    };
                    if rest[end] == b'm' {
                        style.sgr(&escape[1..=end]);
                    }
                    pos += end + 3;
                }
                [b']' | b'_' | b'P' | b'^' | b'X', rest @ ..] => {
                    let terminator = rest.iter().enumerate().find_map(|(i, b)| {
                        if *b == b'\x07' {
                            Some(i + 1)
                        } else if rest[i..].starts_with(b"\x1b\\") {
                            Some(i + 2)
                        } else {
                            None
                        }
                    });
                    let Some(end) = terminator else {
                        break;
                    };
                    pos += end + 2;
                }
                _ => pos += 1 + escape.chars().next().map_or(0, char::len_utf8),
            }
            continue;
        }
        let mut chars = tail.chars();
        let Some(first) = chars.next() else {
            break;
        };
        let mut end = pos + first.len_utf8();
        let mut join = false;
        for ch in chars {
            if !ch.is_control()
                && (join
                    || is_combining_mark(ch)
                    || matches!(
                        ch,
                        '\u{200d}' | '\u{fe0e}' | '\u{fe0f}' | '\u{1f3fb}'..='\u{1f3ff}'
                    ))
            {
                end += ch.len_utf8();
                join = ch == '\u{200d}';
            } else {
                break;
            }
        }
        let glyph = &line[pos..end];
        let start = pos;
        pos = end;
        let glyph_width = grapheme_width(glyph);
        if first.is_control() || glyph_width == 0 || out.len() + glyph_width > width {
            continue;
        }
        let mut fg = style.foreground.unwrap_or(foreground);
        let mut bg = style.background;
        if style.inverse {
            bg = Some(fg);
            fg = style.background.unwrap_or(background);
        }
        if style.dim {
            fg = mix(fg, bg.unwrap_or(background), 0.4);
        }
        let ink = if glyph.trim().is_empty() {
            0
        } else if ".,:;'`-_·".contains(first) {
            2
        } else if ('\u{2500}'..='\u{257f}').contains(&first) {
            3
        } else {
            5
        };
        out.push(ScreenCell {
            text: start..end,
            width: glyph_width,
            fg,
            bg,
            delay: 0.0,
            ink,
        });
        for _ in 1..glyph_width {
            out.push(ScreenCell {
                text: end..end,
                width: 0,
                fg,
                bg,
                delay: 0.0,
                ink,
            });
        }
    }
    out
}

fn prepare_cells(
    model: &Model,
    screen: &[String],
    width: usize,
    height: usize,
    foreground: Rgb,
    background: Rgb,
) -> Vec<Vec<ScreenCell>> {
    let center = model
        .origin
        .map_or((width as f64 / 2.0, height as f64 / 2.0), |(x, y)| {
            (
                x as f64 + f64::from(model.columns) / 2.0,
                y as f64 + f64::from(model.rows) / 4.0,
            )
        });
    let farthest = center
        .0
        .max(width as f64 - center.0)
        .hypot(center.1.max(height as f64 - center.1) * 2.0);
    screen
        .iter()
        .take(height)
        .enumerate()
        .map(|(row, line)| {
            let mut cells = parse_screen_line(line, width, foreground, background);
            let mut delay = 0.0;
            for (col, cell) in cells.iter_mut().enumerate() {
                let ink_seed = hash(cell_seed(row, col));
                if cell.width > 0 {
                    let distance =
                        (col as f64 - center.0).hypot((row as f64 - center.1) * 2.0) / farthest;
                    delay = FLY_START + distance * 0.55 + ink_seed * 0.12;
                }
                cell.delay = delay;
                if cell.ink == 5 {
                    cell.ink = 4 + (ink_seed * 3.0) as u8;
                }
                if model.origin.is_some_and(|(x, y)| {
                    row >= y && row < y.saturating_add(2) && col >= x && col < x.saturating_add(4)
                }) {
                    cell.text = 0..0;
                    cell.bg = None;
                    cell.ink = 0;
                    cell.width = 1;
                }
            }
            cells
        })
        .collect()
}

fn cell_seed(row: usize, col: usize) -> u32 {
    (row as u32).wrapping_mul(65_537).wrapping_add(col as u32)
}

fn dust(cell: &ScreenCell, time: f64, seed: u32) -> (u8, f64) {
    let fade = ((time - cell.delay) / 0.35).clamp(0.0, 1.0);
    let count = (f64::from(cell.ink) * (1.0 - fade)).round() as u32;
    let mut bits = 0u8;
    let phase = ((time * 14.0) as u32)
        .wrapping_mul(131)
        .wrapping_add(seed.wrapping_mul(7919));
    for attempt in 0..32 {
        if bits.count_ones() >= count {
            break;
        }
        bits |= DOT_BITS[(hash(phase.wrapping_add(attempt)) * 8.0) as usize];
    }
    (bits, fade.powf(0.8))
}

fn star(
    index: usize,
    time: f64,
    alpha: f64,
    foreground: Rgb,
    background: Rgb,
) -> Option<(char, Rgb)> {
    let index = index as u32;
    if alpha <= 0.0 || hash(index.wrapping_mul(3).wrapping_add(0x51ed)) >= 0.018 {
        return None;
    }
    let random = |salt: u32| hash(index.wrapping_mul(7).wrapping_add(salt * 0x9e37));
    let tint = STAR_COLORS[(random(2) * 4.0) as usize];
    let flicker = ((0.5 + 0.5 * (time * (0.4 + random(5) * 0.8) + TAU * random(4)).sin()) * 3.0)
        .round()
        / 3.0;
    let strength = (0.1 + random(3) * 0.12) * (0.7 + 0.3 * flicker) * alpha;
    Some((
        braille(DOT_BITS[(random(1) * 8.0) as usize]),
        mix(background, mix(foreground, tint, 0.5), strength),
    ))
}

#[derive(Default)]
struct Frame {
    size: (usize, usize),
    cells: Vec<Vec<ScreenCell>>,
    raster: Raster,
    shuffle: Option<Shuffle>,
    ansi: HashMap<u32, String>,
}

#[derive(Clone, Copy)]
struct Exit {
    started: Instant,
    frozen_time: f64,
}

pub struct EasterEggAnimation {
    theme: Theme,
    mode: TerminalColorMode,
    original: Vec<String>,
    model: Model,
    foreground: Rgb,
    background: Rgb,
    height: usize,
    started: Instant,
    last_tick: Instant,
    exit: Option<Exit>,
    skip: bool,
    frame: RefCell<Frame>,
}

impl EasterEggAnimation {
    pub fn new(
        egg: EasterEgg3d,
        screen: Vec<String>,
        theme: Theme,
        mode: TerminalColorMode,
    ) -> Self {
        let foreground = theme
            .color("text")
            .as_ref()
            .map(color_to_rgb)
            .map_or([210.0; 3], rgb);
        let background = if theme.appearance() == TerminalColorScheme::Light {
            [255.0; 3]
        } else {
            [0.0; 3]
        };
        let accent = theme
            .color("accent")
            .as_ref()
            .map(color_to_rgb)
            .map_or([117.0, 164.0, 228.0], rgb);
        let height = screen.len().max(1);
        let started = Instant::now();
        Self {
            theme,
            mode,
            original: screen,
            model: Model::new(egg, accent),
            foreground,
            background,
            height,
            started,
            last_tick: started,
            exit: None,
            skip: false,
            frame: RefCell::default(),
        }
    }

    /// Supply the terminal's actual OSC colors when known; image composition uses these defaults.
    pub fn with_terminal_colors(mut self, colors: &TerminalColors) -> Self {
        if let Some(foreground) = colors.foreground {
            self.foreground = rgb(foreground);
        }
        if let Some(background) = colors.background {
            self.background = rgb(background);
        }
        self.frame.get_mut().size = (0, 0);
        self
    }

    pub fn set_height(&mut self, height: usize) {
        self.height = height;
    }

    /// Replay the dissolve backwards. Dismissing again returns immediately.
    pub fn dismiss(&mut self) {
        if self.exit.is_some() {
            self.skip = true;
        } else {
            let now = Instant::now();
            self.exit = Some(Exit {
                started: now,
                frozen_time: now.saturating_duration_since(self.started).as_secs_f64(),
            });
        }
    }

    pub fn is_finished(&self) -> bool {
        self.finished_at(Instant::now())
    }

    fn finished_at(&self, now: Instant) -> bool {
        self.skip
            || self.exit.is_some_and(|exit| {
                now.saturating_duration_since(exit.started).as_secs_f64() >= EXIT_DURATION
            })
    }

    pub fn render(&self, width: usize, height: usize) -> Vec<String> {
        self.render_at(width, height, Instant::now())
    }

    fn render_at(&self, width: usize, height: usize, now: Instant) -> Vec<String> {
        if width == 0 || height == 0 {
            return Vec::new();
        }
        if self.finished_at(now) {
            return (0..height)
                .map(|row| {
                    self.original
                        .get(row)
                        .map_or_else(String::new, |line| truncate_to_width(line, width, true))
                })
                .collect();
        }
        let elapsed = now.saturating_duration_since(self.started).as_secs_f64();
        let time = self.exit.map_or(elapsed, |exit| exit.frozen_time);
        let exit_progress = self.exit.map_or(0.0, |exit| {
            (now.saturating_duration_since(exit.started).as_secs_f64() / EXIT_DURATION)
                .clamp(0.0, 1.0)
        });
        let landing = (exit_progress / 0.8).min(1.0);
        let settle = 1.0 - smooth(landing);
        let gather = 1.0 - smooth(exit_progress / 0.5);
        let dissolve_time = if self.exit.is_some() {
            time.min(DUST_END) * (1.0 - exit_progress)
        } else {
            time
        };
        let mut pose = self.model.pose(width, height, fly(time) * settle, time);
        if self.exit.is_some() {
            pose.yaw = lerp(
                pose.yaw,
                (pose.yaw / TAU).ceil() * TAU,
                1.0 - (1.0 - landing).powi(3),
            );
            pose.pitch *= settle;
            pose.roll *= settle;
        }
        let matrix = pose.rotation();
        let light = dot(self.background, [0.2126, 0.7152, 0.0722]) > 128.0;
        let mut frame = self.frame.borrow_mut();
        if frame.size != (width, height) {
            frame.size = (width, height);
            frame.cells = prepare_cells(
                &self.model,
                &self.original,
                width,
                height,
                self.foreground,
                self.background,
            );
        }
        if time >= PUZZLE_START {
            let cycle = ((time - PUZZLE_START) / PUZZLE_CYCLE) as usize;
            if frame
                .shuffle
                .as_ref()
                .is_none_or(|shuffle| shuffle.cycle != cycle)
            {
                frame.shuffle = Some(self.model.next_shuffle(cycle));
            }
        }
        let Frame {
            cells,
            raster,
            shuffle,
            ansi,
            ..
        } = &mut *frame;
        raster.clear(width, height);
        let count = self.model.blocks.len();
        for (index, block) in self.model.blocks.iter().enumerate() {
            let offset = shuffle
                .as_ref()
                .filter(|_| time >= PUZZLE_START)
                .map_or([0.0; 3], |shuffle| {
                    shuffle.offset(index, block.home, count, time)
                });
            let position = std::array::from_fn(|i| f64::from(block.home[i]) + offset[i] * gather);
            raster.block(&self.model, &pose, &matrix, position, block.rgb, light);
        }
        raster.pack(width, self.model.camera, self.background, light);
        let bg_fade = smooth(dissolve_time / 0.5);
        let text_fade = 0.3 * smooth(dissolve_time / 0.6);
        let stars = ((8.0 * smooth((time - 2.5) / 2.0) * (1.0 - smooth(exit_progress / 0.3)))
            .round())
            / 8.0;
        let hint_alpha =
            smooth((time - FLY_START - FLY_DURATION) / 0.5) * (1.0 - smooth(exit_progress / 0.2));
        let hint = if hint_alpha > 0.0 && height >= 2 {
            let bindings = get_keybindings();
            let key = format_key_text(
                bindings
                    .get_keys("tui.select.cancel")
                    .first()
                    .map_or("escape", String::as_str),
                false,
            );
            let hint = format!("{key} to return");
            let key_width = visible_width(&key);
            let hint_width = visible_width(&hint);
            if hint_width <= width {
                let paint = |name| {
                    let color = self
                        .theme
                        .color(name)
                        .as_ref()
                        .map(color_to_rgb)
                        .map_or(self.foreground, rgb);
                    mix(self.background, color, hint_alpha)
                };
                let (left, right) = hint.split_at(key.len());
                let line = format!(
                    "{}{}{}{}\x1b[39m",
                    foreground_ansi(&to_color(paint("muted")), self.mode),
                    left,
                    foreground_ansi(&to_color(paint("dim")), self.mode),
                    right
                );
                Some((
                    (width - hint_width) / 2,
                    key_width + visible_width(right),
                    line,
                ))
            } else {
                None
            }
        } else {
            None
        };
        if ansi.len() > 4096 {
            ansi.clear();
        }
        let mut output = Vec::with_capacity(height);
        for row in 0..height {
            let original = self.original.get(row).map_or("", String::as_str);
            let original_cells = cells.get(row).map_or(&[][..], Vec::as_slice);
            let mut current = [None, None];
            let mut line = String::with_capacity(width + 16);
            let mut col = 0;
            while col < width {
                let index = row * width + col;
                let lit = raster.cells[index];
                let cell = original_cells.get(col).filter(|_| dissolve_time < DUST_END);
                let mut bg = cell
                    .and_then(|cell| cell.bg)
                    .filter(|_| bg_fade < 1.0)
                    .map(|bg| mix(bg, self.background, bg_fade));
                if light {
                    bg = raster
                        .halo(row, col, width, height, bg.unwrap_or(self.background))
                        .or(bg);
                }
                if row == height.saturating_sub(2)
                    && let Some((left, span, hint)) = &hint
                    && col == *left
                {
                    emit(&mut line, &mut current, ansi, None, bg, self.mode);
                    line.push_str(hint);
                    current[0] = None;
                    col += span;
                    continue;
                }
                if lit.bits != 0 {
                    emit(
                        &mut line,
                        &mut current,
                        ansi,
                        Some(lit.quantize()),
                        bg,
                        self.mode,
                    );
                    line.push(braille(lit.bits));
                } else if let Some(cell) = cell.filter(|cell| cell.ink != 0) {
                    let end = col + cell.width;
                    let overlaps_hint = row == height.saturating_sub(2)
                        && hint
                            .as_ref()
                            .is_some_and(|(left, _, _)| col < *left && end > *left);
                    let can_emit = dissolve_time < cell.delay
                        && cell.width > 0
                        && end <= width
                        && !overlaps_hint
                        && raster.cells[index..index + cell.width]
                            .iter()
                            .all(|cell| cell.bits == 0);
                    if can_emit {
                        emit(
                            &mut line,
                            &mut current,
                            ansi,
                            Some(mix(cell.fg, self.background, text_fade)),
                            bg,
                            self.mode,
                        );
                        line.push_str(&original[cell.text.clone()]);
                        col = end;
                        continue;
                    }
                    let (bits, fade) = dust(cell, dissolve_time, cell_seed(row, col));
                    let fg = mix(cell.fg, self.background, lerp(text_fade, 1.0, fade));
                    emit(
                        &mut line,
                        &mut current,
                        ansi,
                        (bits != 0).then_some(fg),
                        bg,
                        self.mode,
                    );
                    line.push(if bits == 0 { ' ' } else { braille(bits) });
                } else if let Some((glyph, fg)) =
                    star(index, elapsed, stars, self.foreground, self.background)
                {
                    emit(&mut line, &mut current, ansi, Some(fg), bg, self.mode);
                    line.push(glyph);
                } else {
                    emit(&mut line, &mut current, ansi, None, bg, self.mode);
                    line.push(' ');
                }
                col += 1;
            }
            line.push_str("\x1b[0m");
            output.push(line);
        }
        output
    }
}

fn to_color([r, g, b]: Rgb) -> Color {
    Color::Rgb(RgbColorValue { r, g, b })
}
fn emit(
    output: &mut String,
    active: &mut [Option<u32>; 2],
    ansi: &mut HashMap<u32, String>,
    fg: Option<Rgb>,
    bg: Option<Rgb>,
    mode: TerminalColorMode,
) {
    for (index, color) in [fg, bg].into_iter().enumerate() {
        if index == 0 && color.is_none() {
            continue;
        }
        let key = color.map(|color| {
            color.into_iter().fold(0u32, |key, c| {
                (key << 8) | c.round().clamp(0.0, 255.0) as u32
            })
        });
        if active[index] == key {
            continue;
        }
        active[index] = key;
        let Some(key) = key else {
            output.push_str("\x1b[49m");
            continue;
        };
        let ansi_key = key * 2 + index as u32;
        output.push_str(ansi.entry(ansi_key).or_insert_with(|| {
            let color = to_color([
                f64::from(key >> 16),
                f64::from((key >> 8) & 255),
                f64::from(key & 255),
            ]);
            if index == 0 {
                foreground_ansi(&color, mode)
            } else {
                background_ansi(&color, mode)
            }
        }));
    }
}

impl Component for EasterEggAnimation {
    fn render(&self, width: usize) -> Vec<String> {
        self.render(width, self.height)
    }

    fn handle_input(&mut self, data: &str) {
        let keys = get_keybindings();
        if keys.matches(data, "tui.select.cancel") || keys.matches(data, "app.clear") {
            self.dismiss();
        }
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<TuiMouseEventResult> {
        if event.event_type == TuiMouseEventType::Click {
            self.dismiss();
        }
        Some(TuiMouseEventResult {
            handled: true,
            render: Some(true),
            ..TuiMouseEventResult::default()
        })
    }

    fn captures_viewport_input(&self) -> bool {
        true
    }

    fn tick(&mut self, now: Instant) -> bool {
        if now.saturating_duration_since(self.last_tick) < FRAME {
            return false;
        }
        self.last_tick = now;
        true
    }

    fn invalidate(&mut self) {
        self.frame.get_mut().size = (0, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_rs_tui::strip_terminal_sequences;
    use std::path::Path;

    #[test]
    fn fullscreen_raster_dissolves_then_restores_screen() -> Result<(), String> {
        let theme = Theme::load(Path::new("."), Some("dark")).map_err(|error| error.to_string())?;
        let mut animation = EasterEggAnimation::new(
            EasterEgg3d::PiLogo { column: 0, row: 0 },
            vec!["█▀█ ".into(), "█▀ █".into(), "prompt 中".into()],
            theme,
            TerminalColorMode::Truecolor,
        );
        let time = animation.started + Duration::from_secs(5);
        let frame = animation.render_at(42, 12, time);
        assert_eq!(frame.len(), 12);
        assert!(frame.iter().all(|line| visible_width(line) == 42));
        assert!(
            frame
                .iter()
                .any(|line| line.contains(|ch| ('\u{2801}'..='\u{28ff}').contains(&ch)))
        );
        assert!(!frame.iter().any(|line| line.contains("prompt")));
        assert!(
            frame
                .iter()
                .any(|line| strip_terminal_sequences(line).contains("to return"))
        );
        animation.exit = Some(Exit {
            started: time,
            frozen_time: 5.0,
        });
        let restored = animation.render_at(42, 12, time + Duration::from_secs(2));
        assert_eq!(
            strip_terminal_sequences(&restored[2]).trim_end(),
            "prompt 中"
        );
        Ok(())
    }

    #[test]
    fn screen_parser_keeps_color_and_wide_glyph_cells() {
        let line = "\x1b[38;2;2;3;4m中a\x1b[0mz";
        let cells = parse_screen_line(line, 5, [255.0; 3], [0.0; 3]);
        assert_eq!(cells.len(), 4);
        assert_eq!(&line[cells[0].text.clone()], "中");
        assert_eq!(cells[1].width, 0);
        assert_eq!(cells[2].fg, [2.0, 3.0, 4.0]);
        assert_eq!(cells[3].fg, [255.0; 3]);
    }
}
