use std::time::{Duration, Instant};

use pi_rs_tui::tui::Component;
use uuid::Uuid;

use super::Theme;

pub(in super::super) const ARMIN_WIDTH: usize = 31;
pub(in super::super) const ARMIN_HEIGHT: usize = 36;
const ROWS: usize = ARMIN_HEIGHT.div_ceil(2);
const CELLS: usize = ARMIN_WIDTH * ROWS;
const BYTES: [u8; 144] = [
    0xff, 0xff, 0xff, 0x7f, 0xff, 0xf0, 0xff, 0x7f, 0xff, 0xed, 0xff, 0x7f, 0xff, 0xdb, 0xff, 0x7f,
    0xff, 0xb7, 0xff, 0x7f, 0xff, 0x77, 0xfe, 0x7f, 0x3f, 0xf8, 0xfe, 0x7f, 0xdf, 0xff, 0xfe, 0x7f,
    0xdf, 0x3f, 0xfc, 0x7f, 0x9f, 0xc3, 0xfb, 0x7f, 0x6f, 0xfc, 0xf4, 0x7f, 0xf7, 0x0f, 0xf7, 0x7f,
    0xf7, 0xff, 0xf7, 0x7f, 0xf7, 0xff, 0xe3, 0x7f, 0xf7, 0x07, 0xe8, 0x7f, 0xef, 0xf8, 0x67, 0x70,
    0x0f, 0xff, 0xbb, 0x6f, 0xf1, 0x00, 0xd0, 0x5b, 0xfd, 0x3f, 0xec, 0x53, 0xc1, 0xff, 0xef, 0x57,
    0x9f, 0xfd, 0xee, 0x5f, 0x9f, 0xfc, 0xae, 0x5f, 0x1f, 0x78, 0xac, 0x5f, 0x3f, 0x00, 0x50, 0x6c,
    0x7f, 0x00, 0xdc, 0x77, 0xff, 0xc0, 0x3f, 0x78, 0xff, 0x01, 0xf8, 0x7f, 0xff, 0x03, 0x9c, 0x78,
    0xff, 0x07, 0x8c, 0x7c, 0xff, 0x0f, 0xce, 0x78, 0xff, 0xff, 0xcf, 0x7f, 0xff, 0xff, 0xcf, 0x78,
    0xff, 0xff, 0xdf, 0x78, 0xff, 0xff, 0xdf, 0x7d, 0xff, 0xff, 0x3f, 0x7e, 0xff, 0xff, 0xff, 0x7f,
];

pub(in super::super) fn is_armin_pixel(x: usize, y: usize) -> bool {
    x < ARMIN_WIDTH
        && y < ARMIN_HEIGHT
        && BYTES[y * ARMIN_WIDTH.div_ceil(8) + x / 8] & (1 << (x % 8)) == 0
}

fn image() -> [char; CELLS] {
    std::array::from_fn(|index| {
        let x = index % ARMIN_WIDTH;
        let y = 2 * (index / ARMIN_WIDTH);
        match (is_armin_pixel(x, y), is_armin_pixel(x, y + 1)) {
            (true, true) => '█',
            (true, false) => '▀',
            (false, true) => '▄',
            (false, false) => ' ',
        }
    })
}

#[derive(Clone, Copy)]
struct RainDrop {
    y: i32,
    settled: usize,
}

enum Effect {
    Typewriter,
    Scanline,
    Rain(Box<[RainDrop; ARMIN_WIDTH]>),
    Reveal {
        order: Box<[usize; CELLS]>,
        rate: usize,
    },
    Crt,
    Glitch,
}

struct Random(u64);

impl Random {
    fn index(&mut self, upper: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 32) as usize % upper
    }
}

pub struct ArminAnimation {
    theme: Theme,
    effect: Effect,
    target: [char; CELLS],
    current: [char; CELLS],
    random: Random,
    frame: usize,
    last: Instant,
    done: bool,
}

impl ArminAnimation {
    pub fn new(theme: Theme) -> Self {
        let seed = Uuid::new_v4().as_u64_pair().1;
        let mut random = Random(seed.max(1));
        let effect = random.index(7);
        Self::with_effect(theme, random, effect)
    }

    fn with_effect(theme: Theme, mut random: Random, index: usize) -> Self {
        const NOISE: [char; 7] = [' ', '░', '▒', '▓', '█', '▀', '▄'];
        let mut current = [' '; CELLS];
        let effect = match index {
            0 => Effect::Typewriter,
            1 => Effect::Scanline,
            2 => Effect::Rain(Box::new(std::array::from_fn(|_| RainDrop {
                y: -(random.index(ROWS * 2) as i32),
                settled: 0,
            }))),
            3 | 6 => {
                let mut order = Box::new(std::array::from_fn(|index| index));
                for index in (1..CELLS).rev() {
                    order.swap(index, random.index(index + 1));
                }
                if index == 6 {
                    current.fill_with(|| NOISE[random.index(NOISE.len())]);
                }
                Effect::Reveal {
                    order,
                    rate: if index == 3 { 15 } else { 20 },
                }
            }
            4 => Effect::Crt,
            _ => Effect::Glitch,
        };
        Self {
            theme,
            effect,
            target: image(),
            current,
            random,
            frame: 0,
            last: Instant::now(),
            done: false,
        }
    }

    pub fn is_animating(&self) -> bool {
        !self.done
    }

    fn next_frame(&mut self) {
        let mut done = false;
        match &mut self.effect {
            effect @ (Effect::Typewriter | Effect::Scanline) => {
                let rate = if matches!(effect, Effect::Typewriter) {
                    3
                } else {
                    ARMIN_WIDTH
                };
                let start = self.frame * rate;
                let end = (start + rate).min(CELLS);
                self.current[start..end].copy_from_slice(&self.target[start..end]);
                done = end == CELLS;
            }
            Effect::Rain(drops) => {
                self.current.fill(' ');
                done = true;
                for (x, drop) in drops.iter_mut().enumerate() {
                    let mut remaining = ROWS - drop.settled;
                    if let Some(target) = (0..remaining)
                        .rev()
                        .find(|&row| self.target[row * ARMIN_WIDTH + x] != ' ')
                    {
                        drop.y += 1;
                        if drop.y >= target as i32 {
                            remaining = target;
                            drop.settled = ROWS - remaining;
                            drop.y = -(self.random.index(5) as i32) - 1;
                        } else if let Ok(y) = usize::try_from(drop.y) {
                            self.current[y * ARMIN_WIDTH + x] = '▓';
                        }
                        done = false;
                    } else {
                        remaining = 0;
                        drop.settled = ROWS;
                    }
                    for row in remaining..ROWS {
                        let index = row * ARMIN_WIDTH + x;
                        self.current[index] = self.target[index];
                    }
                }
            }
            Effect::Reveal { order, rate } => {
                let start = self.frame * *rate;
                let end = (start + *rate).min(CELLS);
                for &index in &order[start..end] {
                    self.current[index] = self.target[index];
                }
                done = end == CELLS;
            }
            Effect::Crt => {
                let mid = ROWS / 2;
                let start = mid.saturating_sub(self.frame);
                let end = (mid + self.frame + 1).min(ROWS);
                let span = start * ARMIN_WIDTH..end * ARMIN_WIDTH;
                self.current[span.clone()].copy_from_slice(&self.target[span]);
                done = end - start == ROWS;
            }
            Effect::Glitch => {
                if self.frame >= 8 {
                    self.current.copy_from_slice(&self.target);
                    done = true;
                } else {
                    for (index, row) in self
                        .current
                        .as_chunks_mut::<ARMIN_WIDTH>()
                        .0
                        .iter_mut()
                        .enumerate()
                    {
                        let offset = (ARMIN_WIDTH + self.random.index(7) - 3) % ARMIN_WIDTH;
                        let shift = self.random.index(10) < 3;
                        let source = if !shift && self.random.index(5) == 0 {
                            self.random.index(ROWS)
                        } else {
                            index
                        };
                        let start = source * ARMIN_WIDTH;
                        row.copy_from_slice(&self.target[start..start + ARMIN_WIDTH]);
                        if shift {
                            row.rotate_left(offset);
                        }
                    }
                }
            }
        }
        self.frame += 1;
        self.done = done;
    }
}

impl Component for ArminAnimation {
    fn render(&self, width: usize) -> Vec<String> {
        if width == 0 {
            return Vec::new();
        }
        let available = width - 1;
        let mut lines = Vec::with_capacity(ROWS + 1);
        let mut add_row = |data: &str, columns: usize| {
            let mut line = String::with_capacity(width + 30);
            line.push(' ');
            line.push_str(&self.theme.fg("accent", data));
            line.extend(std::iter::repeat_n(' ', available - columns));
            lines.push(line);
        };
        let columns = available.min(ARMIN_WIDTH);
        let mut data = String::with_capacity(columns * 3);
        for row in self.current.as_chunks::<ARMIN_WIDTH>().0 {
            data.clear();
            data.extend(row[..columns].iter());
            add_row(&data, columns);
        }
        let message = "ARMIN SAYS HI";
        let columns = available.min(message.len());
        add_row(&message[..columns], columns);
        lines
    }

    fn tick(&mut self, now: Instant) -> bool {
        if self.done {
            return false;
        }
        let frame = if matches!(self.effect, Effect::Glitch) {
            Duration::from_millis(16)
        } else {
            Duration::from_millis(33)
        };
        if now.saturating_duration_since(self.last) < frame {
            return false;
        }
        self.last = now;
        self.next_frame();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn all_armin_effects_finish_on_the_same_bitmap() -> Result<(), String> {
        let theme = Theme::load(Path::new("."), Some("dark")).map_err(|error| error.to_string())?;
        let target = image();
        assert!(target.contains(&'█'));
        assert!(is_armin_pixel(8, 1));
        assert!(!is_armin_pixel(ARMIN_WIDTH, ARMIN_HEIGHT));
        for effect in 0..7 {
            let mut animation = ArminAnimation::with_effect(theme.clone(), Random(42), effect);
            for _ in 0..512 {
                if !animation.is_animating() {
                    break;
                }
                animation.next_frame();
            }
            assert!(!animation.is_animating(), "effect {effect}");
            assert_eq!(animation.current, target);
            assert!(
                animation
                    .render(8)
                    .iter()
                    .all(|line| pi_rs_tui::visible_width(line) == 8)
            );
        }
        Ok(())
    }
}
