//! The activity Ticker's drum physics (upstream `elanor-extra/drum.ts`).

/// Height of an item in px.
pub const FACE: f64 = 24.0;
/// iOS's normal deceleration: velocity is multiplied by this every millisecond.
pub const DECELERATION: f64 = 0.998;
/// A slow release uses a crisper snap.
pub const SNAP_DECELERATION: f64 = 0.986;
/// Velocity (items/ms) below which a release snaps to the nearest item.
pub const FLING: f64 = 0.0015;
/// Rubber band constant past the ends.
pub const RUBBER_BAND: f64 = 0.55;
/// More items than this arriving at once is a catch-up: cut to the newest.
pub const CATCH_UP: i64 = 2;

/// How far a release velocity (items/ms) carries: v · d / (1 − d).
pub fn project(velocity: f64) -> f64 {
    (velocity * DECELERATION) / (1.0 - DECELERATION)
}

/// Past the ends, the drum gives less and less (iOS rubber band, over one item).
pub fn band(position: f64, last: f64) -> f64 {
    let rubber = |over: f64| 1.0 - 1.0 / (over * RUBBER_BAND + 1.0);
    if position < 0.0 {
        -rubber(-position)
    } else if position > last {
        last + rubber(position - last)
    } else {
        position
    }
}

/// The item a release settles on: a fling projects where iOS would stop, a slow release
/// snaps to the nearest item; the result is clamped to `[0, last]`.
pub fn release_target(at: f64, velocity: f64, last: f64) -> i64 {
    let fling = velocity.abs() > FLING;
    let projected = at + if fling { project(velocity) } else { 0.0 };
    (projected.round() as i64).clamp(0, last as i64)
}

/// Whether a new last item rolls, cuts (catch-up), or waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewItems {
    Roll,
    Cut,
    Wait,
}

/// How new items arriving affect the drum (`setLast`).
pub fn on_new_items(added: i64, dragging: bool, recently_interacted: bool) -> NewItems {
    if added == 0 {
        return NewItems::Wait;
    }
    if dragging || recently_interacted {
        return NewItems::Wait;
    }
    if !(0..=CATCH_UP).contains(&added) {
        NewItems::Cut
    } else {
        NewItems::Roll
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_matches_ios() {
        assert!(project(0.001) > 0.0);
        assert!(
            (project(0.001) - 0.001 * DECELERATION / (1.0 - DECELERATION)).abs() < f64::EPSILON
        );
    }

    #[test]
    fn band_is_stiff_past_ends() {
        assert_eq!(band(5.0, 10.0), 5.0);
        assert!(band(-2.0, 10.0) < 0.0 && band(-2.0, 10.0) > -2.0);
        assert!(band(12.0, 10.0) > 10.0 && band(12.0, 10.0) < 12.0);
    }

    #[test]
    fn release_target_snaps_or_flings_and_clamps() {
        assert_eq!(release_target(3.2, 0.0, 10.0), 3);
        assert!(release_target(3.0, 0.01, 10.0) > 3);
        assert_eq!(release_target(3.0, 0.01, 4.0), 4);
        assert_eq!(release_target(-1.0, -0.01, 10.0), 0);
    }

    #[test]
    fn new_items_decisions() {
        assert_eq!(on_new_items(0, false, false), NewItems::Wait);
        assert_eq!(on_new_items(1, false, false), NewItems::Roll);
        assert_eq!(on_new_items(3, false, false), NewItems::Cut);
        assert_eq!(on_new_items(1, true, false), NewItems::Wait);
        assert_eq!(on_new_items(1, false, true), NewItems::Wait);
    }
}
