//! Phone drawer swipe geometry (upstream `drawer-swipe.ts`).

use super::drag_gesture::FLING;

/// The drawer's progress (0 closed, 1 open) after a drag of `along` px, from its state
/// at touch start. `sign` is +1 in LTR, -1 in RTL.
pub fn progress(open: bool, along: f64, width: f64, sign: f64) -> f64 {
    let base = if open { 1.0 } else { 0.0 };
    (base + (along * sign) / width.max(1.0)).clamp(0.0, 1.0)
}

/// The drawer's transform offset in px at a progress.
pub fn offset(progress: f64, width: f64, sign: f64) -> f64 {
    (progress - 1.0) * width * sign
}

/// Whether a drag toward `toward_end` may be taken from a drawer in state `open`.
pub fn may_take(toward_end: bool, open: bool) -> bool {
    toward_end != open
}

/// Whether release settles open: a fling wins, else the halfway position.
pub fn settles_open(velocity: f64, sign: f64, progress: f64) -> bool {
    let toward = velocity * sign;
    if toward.abs() > FLING {
        toward > 0.0
    } else {
        progress > 0.5
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_follows_finger_and_clamps() {
        assert!((progress(false, 100.0, 200.0, 1.0) - 0.5).abs() < f64::EPSILON);
        assert_eq!(progress(false, 999.0, 200.0, 1.0), 1.0);
        assert_eq!(progress(true, -999.0, 200.0, 1.0), 0.0);
    }

    #[test]
    fn rtl_sign_inverts_direction() {
        assert!((progress(false, -100.0, 200.0, -1.0) - 0.5).abs() < f64::EPSILON);
        assert!((progress(true, 100.0, 200.0, -1.0) - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn offset_and_take_rules() {
        assert!((offset(0.5, 200.0, 1.0) + 100.0).abs() < f64::EPSILON);
        assert!(may_take(true, false));
        assert!(!may_take(true, true));
    }

    #[test]
    fn settle_prefers_fling_then_position() {
        assert!(settles_open(0.5, 1.0, 0.1));
        assert!(!settles_open(-0.5, 1.0, 0.9));
        assert!(settles_open(0.0, 1.0, 0.6));
        assert!(!settles_open(0.0, 1.0, 0.4));
    }
}
