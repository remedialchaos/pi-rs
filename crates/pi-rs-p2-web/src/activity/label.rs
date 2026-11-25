//! A step's words and kind icon (upstream `activity/Label.tsx`, `model.ts`).

/// The kind of work a step is, for its icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Command,
    Search,
    Handoff,
    Read,
    Edit,
    Script,
    Agent,
    Artifact,
    File,
    Port,
    Message,
    Tool,
}

impl Kind {
    /// The icon glyph's CSS class suffix, matching upstream's icon components.
    pub fn icon(self) -> &'static str {
        match self {
            Self::Command => "terminal",
            Self::Search => "search",
            Self::Handoff => "refresh",
            Self::Read | Self::File => "file",
            Self::Edit => "pencil",
            Self::Script => "code",
            Self::Agent => "bot",
            Self::Artifact => "artifact",
            Self::Port => "port",
            Self::Message => "conversation",
            Self::Tool => "tool",
        }
    }
}

/// A step's words: a verb and what it acted on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
    pub kind: Kind,
    pub verb: Option<String>,
    pub object: Option<String>,
    pub style: Option<LabelStyle>,
    pub extra: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LabelStyle {
    File,
    Code,
    Words,
}

impl LabelStyle {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Code => "code",
            Self::Words => "words",
        }
    }
}

/// A step for the sheet: its label in the past tense and, if there is more to say, a description.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Described {
    pub label: Label,
    pub description: Option<String>,
    pub mono: bool,
}

/// The plain words of a label, for screen readers and tests (upstream `plain`).
pub fn plain(label: &Label) -> String {
    [
        label.verb.as_deref(),
        label.object.as_deref(),
        label.extra.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_joins_present_parts() {
        let label = Label {
            kind: Kind::Command,
            verb: Some("Ran".into()),
            object: Some("npm test".into()),
            style: Some(LabelStyle::Code),
            extra: Some("2 runs".into()),
        };
        assert_eq!(plain(&label), "Ran npm test 2 runs");
    }

    #[test]
    fn icons_map_to_stable_names() {
        assert_eq!(Kind::Read.icon(), "file");
        assert_eq!(Kind::Command.icon(), "terminal");
        assert_eq!(Kind::Tool.icon(), "tool");
    }
}
