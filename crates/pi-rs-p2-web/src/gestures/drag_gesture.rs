//! One-finger drag geometry (upstream `drag-gesture.ts`).

/// Movement (px) before the axis is decided.
pub const DECIDE: f64 = 6.0;
/// Steepest angle (degrees from the gesture's axis) that still counts as the gesture.
pub const MAX_ANGLE: f64 = 35.0;
/// Velocity over this window (ms) before the release counts as a fling.
pub const VELOCITY_WINDOW: u64 = 100;
/// px/ms: a release faster than this settles in the fling's direction regardless of position.
pub const FLING: f64 = 0.3;
/// Settle easing and the easing's initial slope.
pub const SETTLE_EASING: &str = "cubic-bezier(0.2, 0.8, 0.4, 1)";
const EASING_SLOPE: f64 = 4.0;
const FASTEST: u64 = 120;
const SLOWEST: u64 = 320;

/// How long a settle over `remaining` of `total` px takes, released at `toward` px/ms
/// toward its destination (upstream `settleDuration`).
pub fn settle_duration(remaining: f64, total: f64, toward: f64) -> u64 {
    let slow = FASTEST as f64 + (SLOWEST - FASTEST) as f64 * (remaining / total.max(1.0)).min(1.0);
    if toward > 0.0 {
        slow.min((FASTEST as f64).max(EASING_SLOPE * remaining / toward))
            .round() as u64
    } else {
        slow.round() as u64
    }
}

/// Whether a first move of `(dx, dy)` counts as this axis (`x` or `y`).
///
/// Returns the distance along the axis when it is taken, or `None` when the move is still
/// inside the decision distance, is too steep, or should be left to the page.
pub fn decide_axis(axis: char, dx: f64, dy: f64) -> Option<f64> {
    if dx.hypot(dy) < DECIDE {
        return None;
    }
    let (primary, cross) = if axis == 'x' { (dx, dy) } else { (dy, dx) };
    if cross.abs() > primary.abs() * (MAX_ANGLE * std::f64::consts::PI / 180.0).tan() {
        return None;
    }
    Some(primary)
}

/// Velocity (px/ms) from samples `(position, time_ms)` in the release window.
pub fn velocity(samples: &[(f64, u64)], now: u64) -> f64 {
    let recent: Vec<&(f64, u64)> = samples
        .iter()
        .filter(|(_, t)| now.saturating_sub(*t) <= VELOCITY_WINDOW)
        .collect();
    match (recent.first(), recent.last()) {
        (Some((first, first_t)), Some((last, last_t))) if last_t > first_t => {
            (last - first) / (*last_t - *first_t) as f64
        }
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settle_duration_bounds() {
        assert_eq!(settle_duration(0.0, 100.0, 0.0), FASTEST);
        assert_eq!(settle_duration(100.0, 100.0, 0.0), SLOWEST);
        let fling = settle_duration(100.0, 100.0, 4.0);
        assert!((FASTEST..SLOWEST).contains(&fling));
    }

    #[test]
    fn axis_decision_respects_slop_and_angle() {
        assert_eq!(decide_axis('x', 2.0, 1.0), None);
        assert_eq!(decide_axis('x', 20.0, 2.0), Some(20.0));
        assert_eq!(decide_axis('x', 20.0, 20.0), None);
        assert_eq!(decide_axis('y', 2.0, 20.0), Some(20.0));
    }

    #[test]
    fn velocity_uses_window_and_handles_stillness() {
        let samples = vec![(0.0, 100), (30.0, 130)];
        assert!((velocity(&samples, 130) - 1.0).abs() < f64::EPSILON);
        assert_eq!(velocity(&samples, 500), 0.0);
    }
}
