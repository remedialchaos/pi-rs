//! Convert wheel notch timing to terminal scroll lines.

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub enum WheelScrollLines {
    #[default]
    Auto,
    Fixed(f64),
}

#[derive(Debug)]
pub struct WheelScrollAccelerator {
    lines: WheelScrollLines,
    accelerate: bool,
    last: Option<(i8, f64)>,
    average_gap: Option<f64>,
    carry: f64,
}

impl Default for WheelScrollAccelerator {
    fn default() -> Self {
        let native_acceleration = cfg!(target_os = "macos")
            && ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
                .into_iter()
                .all(|name| std::env::var_os(name).is_none());
        Self::new(WheelScrollLines::Auto, !native_acceleration)
    }
}

impl WheelScrollAccelerator {
    pub fn new(lines: WheelScrollLines, accelerate: bool) -> Self {
        Self {
            lines,
            accelerate,
            last: None,
            average_gap: None,
            carry: 0.0,
        }
    }

    pub fn set_lines(&mut self, lines: WheelScrollLines) {
        *self = Self::new(lines, self.accelerate);
    }

    /// Return the positive line count for a direction of -1 or 1 at `now_ms`.
    pub fn next(&mut self, direction: i8, now_ms: f64) -> usize {
        if let WheelScrollLines::Fixed(lines) = self.lines {
            return if lines.is_finite() {
                lines.floor().max(1.0) as usize
            } else {
                1
            };
        }
        if !self.accelerate {
            return 1;
        }
        let previous = self.last.replace((direction, now_ms));
        let Some(gap) = previous.and_then(|(last_direction, last_time)| {
            let gap = now_ms - last_time;
            (direction == last_direction && gap <= 200.0).then_some(gap)
        }) else {
            self.average_gap = None;
            self.carry = 0.0;
            return 1;
        };
        if gap < 5.0 {
            return 1;
        }
        let average = self
            .average_gap
            .map_or(gap, |previous| (previous + gap) / 2.0);
        self.average_gap = Some(average);
        let lines = (100.0 / average).clamp(1.0, 6.0) + self.carry;
        let whole = lines.floor();
        self.carry = lines - whole;
        whole as usize
    }
}
