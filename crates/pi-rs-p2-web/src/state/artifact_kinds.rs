//! Artifact kinds contributed by plugins (upstream `artifact-kinds.ts`).

use indexmap::IndexMap;

/// One item as the artifacts sheet lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactItem {
    pub id: String,
    pub title: String,
    /// When the item last changed, in milliseconds.
    pub updated: u64,
}

/// An artifact kind descriptor. Hooks are host-provided data; the registry keeps insertion
/// order so a sheet lists kinds in contribution order (upstream `keyedRegistry`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactKind {
    pub title: String,
    pub view: String,
    /// A short label for the button; `None` while loading.
    pub count: Option<usize>,
    /// Whether this person may remove an item (host decides).
    pub removable: bool,
}

/// The artifact kinds by kind, in contribution order.
#[derive(Clone, Debug, Default)]
pub struct ArtifactKinds(IndexMap<String, ArtifactKind>);

impl ArtifactKinds {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, kind: impl Into<String>, value: ArtifactKind) {
        self.0.insert(kind.into(), value);
    }

    pub fn get(&self, kind: &str) -> Option<&ArtifactKind> {
        self.0.get(kind)
    }

    pub fn kinds(&self) -> impl Iterator<Item = (&str, &ArtifactKind)> {
        self.0.iter().map(|(key, value)| (key.as_str(), value))
    }
}

/// Retains the most recently read artifact text for an artifact ID across version reloads
/// so a reader's view neither blanks with a spinner nor loses its scroll position.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArtifactTextCache {
    pub key: String,
    pub value: Option<String>,
    pub error: Option<String>,
}

impl ArtifactTextCache {
    pub fn update(&mut self, id: &str, version: u64, value: String) {
        self.key = format!("{id}@{version}");
        self.value = Some(value);
        self.error = None;
    }

    pub fn visible_for(&self, id: &str) -> Option<&str> {
        let prefix = format!("{id}@");
        self.key
            .starts_with(&prefix)
            .then_some(self.value.as_deref())
            .flatten()
    }
}

/// Measured block metadata captured from a rendered Markdown artifact pane.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockSnapshot {
    pub tag: String,
    pub text: String,
    pub top: f64,
    pub bottom: f64,
}

/// A block identified by its tag and trimmed text, its occurrence index, and viewport offset.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockPlace {
    pub key: String,
    pub index: usize,
    pub offset: f64,
}

/// Reader's scroll position anchored to the top visible block and preceding heading.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReadingPlace {
    pub scroll_top: f64,
    pub top: Option<BlockPlace>,
    pub heading: Option<BlockPlace>,
}

pub fn block_key(tag: &str, text: &str) -> String {
    format!("{}\n{}", tag.to_ascii_uppercase(), text.trim())
}

pub fn is_heading_tag(tag: &str) -> bool {
    matches!(
        tag.to_ascii_uppercase().as_str(),
        "H1" | "H2" | "H3" | "H4" | "H5" | "H6"
    )
}

/// Captures the first block extending below `pane_top + 1` and the nearest heading at or above it.
pub fn capture_reading_place(
    scroll_top: f64,
    pane_top: f64,
    blocks: &[BlockSnapshot],
) -> ReadingPlace {
    let Some(at) = blocks.iter().position(|b| b.bottom > pane_top + 1.0) else {
        return ReadingPlace {
            scroll_top,
            top: None,
            heading: None,
        };
    };
    let placed = |index: usize| {
        let target = &blocks[index];
        let key = block_key(&target.tag, &target.text);
        let occ = blocks[..index]
            .iter()
            .filter(|other| block_key(&other.tag, &other.text) == key)
            .count();
        BlockPlace {
            key,
            index: occ,
            offset: target.top - pane_top,
        }
    };
    let heading = (0..=at)
        .rfind(|&idx| is_heading_tag(&blocks[idx].tag))
        .map(placed);
    ReadingPlace {
        scroll_top,
        top: Some(placed(at)),
        heading,
    }
}

/// Computes the `scrollTop` delta needed after a new Markdown version renders to keep the
/// reader's anchored block (or preceding heading) at its visual position.
pub fn compute_scroll_delta(
    place: &ReadingPlace,
    current_scroll_top: f64,
    pane_top: f64,
    blocks: &[BlockSnapshot],
) -> Option<f64> {
    let scrolled = current_scroll_top - place.scroll_top;
    for wanted in [&place.top, &place.heading].into_iter().flatten() {
        let found = blocks
            .iter()
            .filter(|b| block_key(&b.tag, &b.text) == wanted.key)
            .nth(wanted.index);
        if let Some(block) = found {
            let delta = (block.top - pane_top) - (wanted.offset - scrolled);
            if delta.abs() >= 1.0 {
                return Some(delta);
            }
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(title: &str, view: &str) -> ArtifactKind {
        ArtifactKind {
            title: title.into(),
            view: view.into(),
            count: None,
            removable: false,
        }
    }

    #[test]
    fn kinds_keep_contribution_order() {
        let mut kinds = ArtifactKinds::new();
        kinds.register("html", kind("Pages", "html"));
        kinds.register("board", kind("Boards", "board"));
        assert_eq!(
            kinds.kinds().map(|(key, _)| key).collect::<Vec<_>>(),
            vec!["html", "board"]
        );
        assert_eq!(kinds.get("board").map(|k| k.view.as_str()), Some("board"));
    }

    #[test]
    fn reading_place_preserves_scroll_across_insertions_above_and_falls_back_to_heading() {
        let old_blocks = vec![
            BlockSnapshot {
                tag: "H1".into(),
                text: "Section A".into(),
                top: -120.0,
                bottom: -80.0,
            },
            BlockSnapshot {
                tag: "P".into(),
                text: "Target paragraph".into(),
                top: 10.0,
                bottom: 50.0,
            },
        ];
        let place = capture_reading_place(200.0, 0.0, &old_blocks);

        // Agent inserts text above: Section A and Target paragraph shift down by 140px.
        let new_blocks = vec![
            BlockSnapshot {
                tag: "P".into(),
                text: "Inserted preamble".into(),
                top: 0.0,
                bottom: 140.0,
            },
            BlockSnapshot {
                tag: "H1".into(),
                text: "Section A".into(),
                top: 20.0,
                bottom: 60.0,
            },
            BlockSnapshot {
                tag: "P".into(),
                text: "Target paragraph".into(),
                top: 150.0,
                bottom: 190.0,
            },
        ];
        let delta = compute_scroll_delta(&place, 200.0, 0.0, &new_blocks);
        assert_eq!(delta, Some(140.0));

        // If the target paragraph was edited away, anchor falls back to Section A heading.
        let edited_blocks = vec![
            BlockSnapshot {
                tag: "P".into(),
                text: "Inserted preamble".into(),
                top: 0.0,
                bottom: 140.0,
            },
            BlockSnapshot {
                tag: "H1".into(),
                text: "Section A".into(),
                top: 20.0,
                bottom: 60.0,
            },
            BlockSnapshot {
                tag: "P".into(),
                text: "Rewritten paragraph".into(),
                top: 150.0,
                bottom: 190.0,
            },
        ];
        let fallback_delta = compute_scroll_delta(&place, 200.0, 0.0, &edited_blocks);
        assert_eq!(fallback_delta, Some(140.0));

        let mut cache = ArtifactTextCache::default();
        cache.update("doc-1", 1, "v1 text".into());
        assert_eq!(cache.visible_for("doc-1"), Some("v1 text"));
        assert_eq!(cache.visible_for("doc-2"), None);
    }
}
