//! Where the command menu finds its target when it opens (upstream `command-menu/targets.ts`).

use super::layers::CommandTarget;

/// Candidate targets written by rows and the main area, read only when the menu opens.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TargetSources {
    pub hovered: Option<CommandTarget>,
    pub focused: Option<CommandTarget>,
    pub route: Option<CommandTarget>,
}

/// The target the menu captures as it opens, by priority: hovered, focused, the route's.
pub fn current_target(sources: &TargetSources) -> Option<CommandTarget> {
    sources
        .hovered
        .clone()
        .or_else(|| sources.focused.clone())
        .or_else(|| sources.route.clone())
}

/// Whether a row still holds the given target (upstream `same`).
pub fn same(target: Option<&CommandTarget>, target_type: &str, id: &str) -> bool {
    target.is_some_and(|value| value.target_type == target_type && value.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(id: &str) -> CommandTarget {
        CommandTarget {
            target_type: "session".into(),
            id: id.into(),
            kind: "conversation".into(),
            label: "Demo".into(),
        }
    }

    #[test]
    fn priority_is_hovered_then_focused_then_route() {
        let mut sources = TargetSources {
            route: Some(target("route")),
            ..Default::default()
        };
        assert_eq!(current_target(&sources).map(|t| t.id), Some("route".into()));
        sources.focused = Some(target("focused"));
        assert_eq!(
            current_target(&sources).map(|t| t.id),
            Some("focused".into())
        );
        sources.hovered = Some(target("hovered"));
        assert_eq!(
            current_target(&sources).map(|t| t.id),
            Some("hovered".into())
        );
    }

    #[test]
    fn same_compares_type_and_id() {
        assert!(same(Some(&target("a")), "session", "a"));
        assert!(!same(Some(&target("a")), "session", "b"));
        assert!(!same(None, "session", "a"));
    }
}
