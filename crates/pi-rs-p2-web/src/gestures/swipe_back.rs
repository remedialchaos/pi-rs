//! Swipe back in a drawer stack (upstream `elanor-extra/swipe-back.ts`).

use super::sheet_drag::rubber_band;

/// A swipe completes past this share of the width, or faster than this (px/ms).
pub const BACK_DISTANCE: f64 = 0.4;
pub const BACK_VELOCITY: f64 = 0.45;
/// Rubber band of a pull towards the inline start.
pub const OVERPULL_RESISTANCE: f64 = 0.15;
/// Movement (px) before a touch is read as horizontal or vertical.
pub const SLOP: f64 = 8.0;
/// Without a move this long (ms) before release, the finger was still: no flick.
pub const STILL: u64 = 100;

/// The drag's axis once movement passes `SLOP`, or `None` while undecided / vertical.
pub fn axis(distance: f64, dy: f64, sideways: bool) -> Option<&'static str> {
    if distance.abs() < SLOP && dy.abs() < SLOP {
        return None;
    }
    Some(if distance.abs() > dy.abs() && !sideways {
        "x"
    } else {
        "none"
    })
}

/// Whether the release goes back: distance or velocity thresholds.
pub fn goes_back(distance: f64, width: f64, velocity: f64) -> bool {
    distance / width.max(1.0) > BACK_DISTANCE || velocity > BACK_VELOCITY
}

/// The offset shown for a drag: forward follows the finger, backward has a rubber band.
pub fn swipe_offset(distance: f64, width: f64) -> f64 {
    if distance >= 0.0 {
        distance
    } else {
        rubber_band(distance, width, OVERPULL_RESISTANCE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axis_needs_slop_then_picks_horizontal() {
        assert_eq!(axis(5.0, 1.0, false), None);
        assert_eq!(axis(20.0, 3.0, false), Some("x"));
        assert_eq!(axis(5.0, 20.0, false), Some("none"));
        assert_eq!(axis(20.0, 3.0, true), Some("none"));
    }

    #[test]
    fn distance_or_velocity_completes() {
        assert!(goes_back(50.0, 100.0, 0.0));
        assert!(goes_back(10.0, 100.0, 0.5));
        assert!(!goes_back(10.0, 100.0, 0.1));
    }

    #[test]
    fn overpull_is_banded() {
        assert_eq!(swipe_offset(20.0, 100.0), 20.0);
        assert!(swipe_offset(-20.0, 100.0) < 0.0);
        assert!(swipe_offset(-20.0, 100.0) > -20.0);
    }
}
