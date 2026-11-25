//! A panel size dragged with a handle (upstream `drag-size.ts`).

/// Reads a size from storage: a non-numeric or non-positive value reads as absent.
pub fn read_size(stored: Option<&str>) -> Option<u32> {
    stored
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|size| *size > 0)
}

/// Clamps a dragged size to the caller's bounds and rounds it to whole pixels.
pub fn clamp_size(moved: f64, min: f64, max: f64) -> u32 {
    moved.round().clamp(min, max) as u32
}

/// The CSS custom property declaration setting the size (`--variable: <size>px`).
pub fn size_style(variable: &str, size: u32) -> String {
    format!("{variable}: {size}px")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_only_positive_numbers() {
        assert_eq!(read_size(Some("240")), Some(240));
        assert_eq!(read_size(Some("0")), None);
        assert_eq!(read_size(Some("NaN")), None);
        assert_eq!(read_size(None), None);
    }

    #[test]
    fn clamps_and_rounds() {
        assert_eq!(clamp_size(123.6, 100.0, 200.0), 124);
        assert_eq!(clamp_size(10.0, 100.0, 200.0), 100);
        assert_eq!(clamp_size(999.0, 100.0, 200.0), 200);
    }

    #[test]
    fn style_declaration() {
        assert_eq!(size_style("--sidebar-width", 240), "--sidebar-width: 240px");
    }
}
