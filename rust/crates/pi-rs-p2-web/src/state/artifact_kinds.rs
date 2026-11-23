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
}
