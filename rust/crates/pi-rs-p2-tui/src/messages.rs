pub use crate::components::transcript::Transcript;
use crate::components::transcript::TranscriptView;
use crate::theme::{Theme, ThemeRef};

#[derive(Default)]
pub struct TranscriptComponentOptions {
    pub theme: Option<ThemeRef>,
    pub tool_renderers: indexmap::IndexMap<String, crate::tools::ToolRenderers>,
    pub entry_renderers: indexmap::IndexMap<String, crate::components::TuiEntryRenderer>,
}
pub fn transcript_component(
    transcript: &Transcript,
    show_thinking: bool,
    expanded_tools: bool,
    options: TranscriptComponentOptions,
) -> TranscriptView {
    TranscriptView {
        theme: options
            .theme
            .unwrap_or_else(|| ThemeRef::new(Theme::default())),
        tool_renderers: options.tool_renderers,
        entry_renderers: options.entry_renderers,
        expanded: expanded_tools,
        show_thinking,
        transcript: transcript.clone(),
        rows: transcript
            .items
            .iter()
            .map(|_| Default::default())
            .collect(),
    }
}
