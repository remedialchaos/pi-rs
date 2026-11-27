pub use crate::components::transcript::Transcript;
use crate::components::transcript::{KeyTextFn, TranscriptView};
use crate::theme::{Theme, ThemeRef};

#[derive(Default)]
pub struct TranscriptComponentOptions {
    pub theme: Option<ThemeRef>,
    pub tool_renderers: indexmap::IndexMap<String, crate::tools::ToolRenderers>,
    pub entry_renderers: indexmap::IndexMap<String, crate::components::TuiEntryRenderer>,
    pub quiet_tools: Option<bool>,
    pub cwd: String,
    pub output_pad: Option<usize>,
    pub mermaid_settings: Option<crate::preferences::TuiSettings>,
    pub key_text: Option<KeyTextFn>,
}
pub fn transcript_component(
    transcript: &Transcript,
    show_thinking: bool,
    expanded_tools: bool,
    options: TranscriptComponentOptions,
) -> TranscriptView {
    let quiet_tools = options.quiet_tools.unwrap_or_else(|| {
        crate::tools::plugin_tool_calls_is_quiet(options.mermaid_settings.as_ref())
    });
    TranscriptView {
        theme: options
            .theme
            .unwrap_or_else(|| ThemeRef::new(Theme::default())),
        tool_renderers: options.tool_renderers,
        entry_renderers: options.entry_renderers,
        expanded: expanded_tools,
        show_thinking,
        quiet_tools,
        cwd: options.cwd,
        output_pad: options.output_pad.unwrap_or(1),
        mermaid_settings: options.mermaid_settings,
        key_text: options.key_text,
        transcript: transcript.clone(),
        rows: transcript
            .items
            .iter()
            .map(|_| Default::default())
            .collect(),
    }
}
