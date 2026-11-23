//! Scroll-anchor block search (upstream `anchor.ts`).

/// Something with a layout box, such as a transcript block.
pub trait Box {
    /// The block's top and bottom in the scroller's coordinate space.
    fn bounds(&self) -> (f64, f64);
}

/// A plain box for tests and non-DOM hosts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub top: f64,
    pub bottom: f64,
}

impl Box for Rect {
    fn bounds(&self) -> (f64, f64) {
        (self.top, self.bottom)
    }
}

/// The first `count` blocks not entirely above `top`, with their tops.
///
/// Upstream `firstVisibleBlocks`. Blocks are stacked top to bottom, so a binary search
/// finds the first one in about `log2(n)` reads rather than reading every rectangle (which
/// costs hundreds of milliseconds on a long session, on every resize). A block absent from
/// the slice is treated as above `top`, matching upstream's `?? top` fallback.
pub fn first_visible_blocks<T: Box>(blocks: &[T], top: f64, count: usize) -> Vec<(usize, &T, f64)> {
    let mut low = 0;
    let mut high = blocks.len();
    while low < high {
        let middle = (low + high) >> 1;
        let bottom = blocks.get(middle).map(|b| b.bounds().1).unwrap_or(top);
        if bottom <= top {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    blocks[low..]
        .iter()
        .take(count)
        .enumerate()
        .map(|(offset, element)| (low + offset, element, element.bounds().0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rects() -> Vec<Rect> {
        vec![
            Rect {
                top: 0.0,
                bottom: 10.0,
            },
            Rect {
                top: 10.0,
                bottom: 20.0,
            },
            Rect {
                top: 20.0,
                bottom: 30.0,
            },
            Rect {
                top: 30.0,
                bottom: 40.0,
            },
        ]
    }

    #[test]
    fn skips_blocks_entirely_above_top() {
        let blocks = rects();
        let found = first_visible_blocks(&blocks, 25.0, 4);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].0, 2);
        assert_eq!(found[0].2, 20.0);
        assert_eq!(found[1].0, 3);
    }

    #[test]
    fn a_block_straddling_top_is_kept() {
        let blocks = rects();
        let found = first_visible_blocks(&blocks, 5.0, 2);
        assert_eq!(found[0].0, 0);
        assert_eq!(found[0].2, 0.0);
        assert_eq!(found[1].0, 1);
    }

    #[test]
    fn limits_to_count() {
        let blocks = rects();
        let found = first_visible_blocks(&blocks, 0.0, 2);
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn empty_and_fully_scrolled_past() {
        assert!(first_visible_blocks::<Rect>(&[], 0.0, 4).is_empty());
        assert!(first_visible_blocks(&rects(), 100.0, 4).is_empty());
    }
}
