//! `forward`: a view's members taken from a store's reference. Most remote objects own nothing: their
//! factory gets a store and hands its members on to one audience. The contract still lists that
//! audience's members explicitly; `forward` fills them from the same-named members of the store, so
//! no member is written a second time.
//!
//! Upstream's `forward` reads a Chord contract's member names. Native views carry a name list; the
//! rule is the same: `own`'s member when it gives one, else the store's same-named member.

use indexmap::IndexMap;
use serde_json::Value;

/// A view's members: values by name, in the order the contract lists them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Members(IndexMap<String, Value>);

impl Members {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        self.0.get(name)
    }

    pub fn insert(&mut self, name: impl Into<String>, value: Value) -> Option<Value> {
        self.0.insert(name.into(), value)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<(String, Value)> for Members {
    fn from_iter<T: IntoIterator<Item = (String, Value)>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// Why a view cannot be filled.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ForwardError {
    #[error("forward: neither the store nor `own` gives member \"{0}\"")]
    Missing(String),
}

/// The members `names` of a view: `own`'s when it gives one, else the same-named member of `store`.
pub fn forward(
    store: &Members,
    own: Option<&Members>,
    names: &[&str],
) -> Result<Members, ForwardError> {
    let mut members = Members::new();
    for name in names {
        let value = own
            .and_then(|own| own.get(name))
            .or_else(|| store.get(name))
            .ok_or_else(|| ForwardError::Missing((*name).to_string()))?;
        members.insert(*name, value.clone());
    }
    Ok(members)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn members(entries: &[(&str, Value)]) -> Members {
        entries
            .iter()
            .map(|(name, value)| ((*name).to_string(), value.clone()))
            .collect()
    }

    #[test]
    fn a_forwarded_view_takes_the_stores_members_and_keeps_the_contracts_order() {
        let store = members(&[
            ("pid", json!(7)),
            ("report", json!("fn")),
            ("crash", json!("fn")),
        ]);
        let view = forward(&store, None, &["pid", "report", "crash"]).expect("forward");
        assert_eq!(view.len(), 3);
        assert_eq!(view.names().collect::<Vec<_>>(), ["pid", "report", "crash"]);
        assert_eq!(view.get("pid"), Some(&json!(7)));
    }

    #[test]
    fn own_members_win_over_the_stores() {
        let store = members(&[("session", json!({"id": "s1"})), ("report", json!("store"))]);
        let own = members(&[("report", json!("own"))]);
        let view = forward(&store, Some(&own), &["session", "report"]).expect("forward");
        assert_eq!(view.get("report"), Some(&json!("own")));
        assert_eq!(view.get("session"), Some(&json!({"id": "s1"})));
    }

    #[test]
    fn a_member_nobody_gives_is_refused() {
        let store = members(&[("pid", json!(7))]);
        assert_eq!(
            forward(&store, None, &["pid", "crash"]),
            Err(ForwardError::Missing("crash".into()))
        );
    }
}
