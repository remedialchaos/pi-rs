//! Virtualized transcript layout math (upstream `virtual-list.ts`, `use-chat-scroll.ts`).

use std::collections::HashMap;

/// Default estimated row height in pixels before measurement.
pub const DEFAULT_ROW_HEIGHT_PX: f64 = 72.0;
/// Overscan above and below the viewport in pixels.
pub const MIN_OVERSCAN_PX: f64 = 400.0;
/// Distance from bottom within which the transcript stays pinned to the tail.
pub const PIN_THRESHOLD_PX: f64 = 48.0;

/// Windowed row slice + spacers computed by `RowLayout`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VirtualWindow {
    /// Inclusive first rendered index (`None` when list is empty).
    pub start: usize,
    /// Exclusive end rendered index.
    pub end: usize,
    pub top_spacer_px: f64,
    pub bottom_spacer_px: f64,
    pub total_height_px: f64,
}

/// Tracks estimated vs measured row heights per conversation key.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RowLayout {
    keys: Vec<String>,
    measured: HashMap<String, f64>,
    default_height: f64,
}

impl RowLayout {
    pub fn new(default_height: f64) -> Self {
        Self {
            keys: Vec::new(),
            measured: HashMap::new(),
            default_height: default_height.max(16.0),
        }
    }

    pub fn set_keys(&mut self, keys: Vec<String>) {
        self.keys = keys;
    }

    pub fn record_height(&mut self, key: &str, height_px: f64) {
        if height_px.is_finite() && height_px > 0.0 {
            self.measured.insert(key.to_owned(), height_px);
        }
    }

    pub fn height_of(&self, index: usize) -> f64 {
        self.keys
            .get(index)
            .and_then(|k| self.measured.get(k))
            .copied()
            .unwrap_or(self.default_height)
    }

    pub fn total_height(&self) -> f64 {
        (0..self.keys.len()).map(|i| self.height_of(i)).sum()
    }

    /// Computes `[start, end)` and top/bottom spacer heights for `(scroll_top, viewport_height)`.
    pub fn window(&self, scroll_top: f64, viewport_height: f64, overscan_px: f64) -> VirtualWindow {
        let len = self.keys.len();
        if len == 0 {
            return VirtualWindow {
                start: 0,
                end: 0,
                top_spacer_px: 0.0,
                bottom_spacer_px: 0.0,
                total_height_px: 0.0,
            };
        }
        let overscan = overscan_px.max(MIN_OVERSCAN_PX);
        let min_y = (scroll_top - overscan).max(0.0);
        let max_y = scroll_top + viewport_height.max(1.0) + overscan;

        let mut y = 0.0;
        let mut start = len;
        let mut top_spacer_px = 0.0;
        let mut end = len;
        let mut bottom_spacer_px = 0.0;

        for idx in 0..len {
            let h = self.height_of(idx);
            let next_y = y + h;
            if start == len && next_y >= min_y {
                start = idx;
                top_spacer_px = y;
            }
            if start < len && y > max_y && end == len {
                end = idx;
            }
            if end < len {
                bottom_spacer_px += h;
            }
            y = next_y;
        }
        if start == len {
            start = len.saturating_sub(1);
            top_spacer_px = (y - self.height_of(start)).max(0.0);
            end = len;
            bottom_spacer_px = 0.0;
        }

        VirtualWindow {
            start,
            end,
            top_spacer_px,
            bottom_spacer_px,
            total_height_px: y,
        }
    }

    /// Returns whether the scroller is close enough to the bottom to remain pinned.
    pub fn is_pinned(scroll_top: f64, viewport_height: f64, scroll_height: f64) -> bool {
        let from_end = scroll_height - viewport_height - scroll_top;
        from_end <= PIN_THRESHOLD_PX
    }
}

/// Guards transcript measurement and range updates when the chat scroller has no layout box
/// (e.g., `display: none` on phone/narrow-window viewports or when an artifact covers the chat tab).
#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptScrollState {
    pub layout: RowLayout,
    pub rendered: (usize, usize),
    pub viewport_px: f64,
    pub scroll_top_px: f64,
    pub hidden: bool,
    pub sync_count: usize,
    pub range_updates: usize,
}

impl TranscriptScrollState {
    pub fn new(default_height: f64, viewport_px: f64) -> Self {
        Self {
            layout: RowLayout::new(default_height),
            rendered: (0, 0),
            viewport_px,
            scroll_top_px: 0.0,
            hidden: false,
            sync_count: 0,
            range_updates: 0,
        }
    }

    pub fn set_hidden(&mut self, hidden: bool) {
        self.hidden = hidden;
    }

    /// Commits a rendered range and updates visible layout unless the transcript is hidden.
    pub fn committed(&mut self, start: usize, end: usize) -> bool {
        self.rendered = (start, end);
        if self.hidden {
            return false;
        }
        self.sync_count += 1;
        true
    }

    /// Applies scroll/viewport updates unless the transcript has no layout box.
    pub fn sync(&mut self, scroll_top_px: f64, viewport_px: f64) -> bool {
        if self.hidden {
            return false;
        }
        self.scroll_top_px = scroll_top_px;
        self.viewport_px = viewport_px;
        self.sync_count += 1;
        true
    }

    /// Records measured row heights while visible; ignores 0-height measurements while hidden.
    pub fn measured(&mut self, entries: &[(&str, f64)]) -> bool {
        if self.hidden {
            return false;
        }
        for &(key, h) in entries {
            self.layout.record_height(key, h);
        }
        let win = self
            .layout
            .window(self.scroll_top_px, self.viewport_px, MIN_OVERSCAN_PX);
        if (win.start, win.end) != self.rendered {
            self.rendered = (win.start, win.end);
            self.range_updates += 1;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_window_computes_visible_slice_and_spacers() {
        let mut layout = RowLayout::new(100.0);
        layout.set_keys((0..30).map(|i| format!("r{i}")).collect());
        layout.record_height("r0", 150.0);
        assert!((layout.total_height() - 3050.0).abs() < f64::EPSILON);

        let win = layout.window(1500.0, 500.0, 200.0);
        assert!(win.start > 0);
        assert!(win.end < 30);
        assert!(win.top_spacer_px > 0.0);
        assert!(win.bottom_spacer_px > 0.0);
        let rendered: f64 = (win.start..win.end).map(|i| layout.height_of(i)).sum();
        assert!((win.top_spacer_px + rendered + win.bottom_spacer_px - 3050.0).abs() < 1e-6);
    }

    #[test]
    fn pinning_threshold_detects_bottom() {
        assert!(RowLayout::is_pinned(960.0, 400.0, 1400.0));
        assert!(!RowLayout::is_pinned(800.0, 400.0, 1400.0));
    }

    #[test]
    fn hidden_transcript_stops_measuring_on_phone_narrow_desktop_and_front_tab() {
        // Covers phone (390x844), narrow desktop (<700px: 600x900), and wide desktop (1440x900
        // when a view becomes the chat's front tab and hides the transcript with display:none).
        for &(viewport_w, viewport_h) in &[(390.0, 844.0), (600.0, 900.0), (1440.0, 900.0)] {
            let mut state = TranscriptScrollState::new(DEFAULT_ROW_HEIGHT_PX, viewport_h);
            let keys: Vec<String> = (0..120).map(|i| format!("row-{i}")).collect();
            state.layout.set_keys(keys.clone());
            assert!(state.committed(100, 120));
            assert!(state.measured(&[("row-100", 64.0), ("row-101", 88.0)]));
            let prev_syncs = state.sync_count;
            let prev_updates = state.range_updates;
            let prev_range = state.rendered;

            // Pane becomes display:none (getClientRects().length == 0).
            state.set_hidden(true);
            assert!(
                !state.committed(prev_range.0, prev_range.1),
                "width {viewport_w}: committed must no-op when hidden"
            );
            assert!(
                !state.sync(0.0, 0.0),
                "width {viewport_w}: sync must no-op when hidden"
            );
            let zero_rows: Vec<(&str, f64)> =
                keys.iter().take(10).map(|k| (k.as_str(), 0.0)).collect();
            assert!(
                !state.measured(&zero_rows),
                "width {viewport_w}: measured must ignore 0-height hidden rows"
            );
            assert_eq!(state.sync_count, prev_syncs);
            assert_eq!(state.range_updates, prev_updates);
            assert_eq!(state.rendered, prev_range);
            // Existing cached heights stay intact instead of being wiped by 0px hidden rows.
            assert!((state.layout.height_of(100) - 64.0).abs() < f64::EPSILON);

            // Returning to chat resumes measurement on resize.
            state.set_hidden(false);
            assert!(state.sync(1200.0, viewport_h));
            assert!(state.measured(&[("row-100", 70.0)]));
            assert!((state.layout.height_of(100) - 70.0).abs() < f64::EPSILON);
        }
    }
}
