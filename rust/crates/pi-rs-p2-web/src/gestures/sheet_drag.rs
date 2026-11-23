//! Bottom-sheet drag geometry (upstream `elanor-extra/sheet-drag.ts` + `sheet-swipe.ts`).

use super::drag_gesture::FLING;

/// Rubber band of a drag down: offset = (1 - 1 / (distance·k/size + 1)) · size.
pub fn rubber_band(distance: f64, size: f64, constant: f64) -> f64 {
    if size <= 0.0 {
        return 0.0;
    }
    distance.signum() * (1.0 - 1.0 / ((distance.abs() * constant) / size + 1.0)) * size
}

/// Share of the sheet's height past which a slow release closes it.
pub const CLOSE_DISTANCE: f64 = 0.3;
/// A flick down faster than this (px/ms) closes it.
pub const CLOSE_VELOCITY: f64 = 0.7;
/// Upward drags move this share of the finger's distance.
pub const RESIST: f64 = 0.2;

/// The sheet's offset for a drag `along` (upward drags are resisted).
pub fn sheet_offset(along: f64) -> f64 {
    if along > 0.0 { along } else { along * RESIST }
}

/// Whether a release closes the sheet.
pub fn closes(velocity: f64, offset: f64, height: f64) -> bool {
    if velocity.abs() > FLING {
        velocity > 0.0
    } else {
        offset / height.max(1.0) > CLOSE_DISTANCE
    }
}

/// Scrim opacity for an offset (upstream `1 - offset/height`, floored at zero).
pub fn scrim_opacity(offset: f64, height: f64) -> f64 {
    (1.0 - offset / height.max(1.0)).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rubber_band_is_monotonic_and_bounded() {
        assert_eq!(rubber_band(10.0, 0.0, 0.8), 0.0);
        let small = rubber_band(20.0, 100.0, 0.8);
        let large = rubber_band(200.0, 100.0, 0.8);
        assert!(small > 0.0 && large > small && large < 100.0);
        assert!(rubber_band(-20.0, 100.0, 0.15) < 0.0);
    }

    #[test]
    fn offsets_resist_upward_drags() {
        assert_eq!(sheet_offset(50.0), 50.0);
        assert_eq!(sheet_offset(-50.0), -10.0);
    }

    #[test]
    fn close_decision() {
        assert!(closes(1.0, 0.0, 100.0));
        assert!(!closes(-1.0, 90.0, 100.0));
        assert!(closes(0.0, 40.0, 100.0));
        assert!(!closes(0.0, 20.0, 100.0));
    }

    #[test]
    fn scrim_fades_out() {
        assert!((scrim_opacity(0.0, 100.0) - 1.0).abs() < f64::EPSILON);
        assert_eq!(scrim_opacity(150.0, 100.0), 0.0);
    }
}
