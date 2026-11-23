//! Terminal pane and voice audio status/controls (upstream `Viewer.tsx`, voice controls).

use leptos::prelude::*;

/// A terminal's connection status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalStatus {
    Connecting,
    Live,
    Closed,
}

impl TerminalStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Live => "live",
            Self::Closed => "closed",
        }
    }
}

/// An embedded terminal pane with a status badge and a scrollable output area.
#[component]
pub fn Terminal(
    status: TerminalStatus,
    lines: RwSignal<Vec<String>>,
    title: String,
) -> impl IntoView {
    view! {
        <div class="terminal-pane" data-status=status.as_str() data-testid="terminal">
            <header class="terminal-head">
                <span class="terminal-title">{title}</span>
                <span class="terminal-status" data-status=status.as_str()>{status.as_str()}</span>
            </header>
            <pre class="terminal-output" role="log">
                <For
                    each=move || lines.get()
                    key=|line| line.clone()
                    children={move |line: String| view! { <span class="terminal-line">{line}</span> }}
                />
            </pre>
        </div>
    }
}

/// Voice capture/playback status (upstream voice audio controls).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceState {
    Idle,
    Listening,
    Processing,
    Speaking,
}

impl VoiceState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Listening => "listening",
            Self::Processing => "processing",
            Self::Speaking => "speaking",
        }
    }
}

/// Voice controls: a status indicator and a push-to-talk toggle.
#[component]
pub fn Voice(state: RwSignal<VoiceState>, on_toggle: Callback<()>) -> impl IntoView {
    view! {
        <div class="voice-controls" data-state=move || state.get().as_str()>
            <span class="voice-status" aria-live="polite">{move || state.get().as_str()}</span>
            <button type="button" class="voice-toggle" aria-label="Toggle voice" on:click=move |_| on_toggle.run(())>
                "Mic"
            </button>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_strings() {
        assert_eq!(TerminalStatus::Live.as_str(), "live");
        assert_eq!(VoiceState::Speaking.as_str(), "speaking");
    }
}
