//! The Recently-run command IDs (upstream `command-menu/recents.ts`).

/// Storage key and cap, matching upstream.
pub const KEY: &str = "p2-command-recents-v1";
pub const LIMIT: usize = 8;

/// The recently run command ids, newest first. Storage access is a host concern; this
/// struct holds the portable state and the rules for reading and recording.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recents(Vec<String>);

impl Recents {
    /// Reads recents from a stored JSON value: an unreadable or foreign value reads as none.
    pub fn read(stored: Option<&str>) -> Self {
        let Some(stored) = stored else {
            return Self::default();
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(stored) else {
            return Self::default();
        };
        let Some(array) = value.as_array() else {
            return Self::default();
        };
        let ids = array
            .iter()
            .filter_map(|entry| entry.as_str())
            .take(LIMIT)
            .map(str::to_owned)
            .collect();
        Self(ids)
    }

    /// Puts `id` first, keeping the last `LIMIT`.
    pub fn record(&mut self, id: impl Into<String>) {
        let id = id.into();
        self.0.retain(|recent| recent != &id);
        self.0.insert(0, id);
        self.0.truncate(LIMIT);
    }

    /// The ids, newest first.
    pub fn ids(&self) -> &[String] {
        &self.0
    }

    /// The JSON to persist.
    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.0).unwrap_or_else(|_| "[]".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_newest_first_and_deduplicates() {
        let mut recents = Recents::default();
        recents.record("a");
        recents.record("b");
        recents.record("a");
        assert_eq!(recents.ids(), &["a", "b"]);
    }

    #[test]
    fn caps_at_limit() {
        let mut recents = Recents::default();
        for i in 0..(LIMIT + 3) {
            recents.record(format!("c{i}"));
        }
        assert_eq!(recents.ids().len(), LIMIT);
        assert_eq!(recents.ids()[0], "c10");
    }

    #[test]
    fn read_rejects_foreign_values() {
        assert_eq!(
            Recents::read(Some(r#"{"not":"an array"}"#)),
            Recents::default()
        );
        assert_eq!(Recents::read(Some("[1,2,3]")), Recents::default());
        let parsed = Recents::read(Some(r#"["b", 42, "a"]"#));
        assert_eq!(parsed.ids(), &["b", "a"]);
    }

    #[test]
    fn json_round_trip() {
        let mut recents = Recents::default();
        recents.record("x");
        assert_eq!(Recents::read(Some(&recents.to_json())), recents);
    }
}
