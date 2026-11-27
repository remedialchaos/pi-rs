//! Terminal pane (multi-tab ANSI/VT screen, keyboard input, resize) and live WebAudio Voice controls
//! (upstream `apps/p2/plugins/terminals/Terminal.tsx`, `apps/p2/plugins/voice/web.tsx`).

use crate::browser_plugins::VoiceView;
use crate::state::voice_audio::{DEFAULT_PAUSE_SECS, MAX_PAUSE_SECS, MIN_PAUSE_SECS};
use crate::state::vt::encode_terminal_key;
use leptos::prelude::*;

/// A terminal's connection status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalStatus {
    Connecting,
    Live,
    Closed,
}

impl TerminalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Live => "live",
            Self::Closed => "closed",
        }
    }
}

/// Metadata for a single terminal tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalTabInfo {
    pub id: String,
    pub title: String,
    pub cols: u16,
    pub rows: u16,
}

/// Interactive multi-tab PTY terminal pane with ANSI/VT HTML output, keyboard input, and resize.
#[component]
pub fn Terminal(
    status: Signal<TerminalStatus>,
    lines: RwSignal<Vec<String>>,
    title: Signal<String>,
    #[prop(optional)] tabs: Option<Signal<Vec<TerminalTabInfo>>>,
    #[prop(optional)] active_id: Option<Signal<Option<String>>>,
    #[prop(optional)] cols: Option<RwSignal<u16>>,
    #[prop(optional)] rows: Option<RwSignal<u16>>,
    #[prop(optional)] on_new_tab: Option<Callback<()>>,
    #[prop(optional)] on_select_tab: Option<Callback<String>>,
    #[prop(optional)] on_close_tab: Option<Callback<String>>,
    #[prop(optional)] on_key_input: Option<Callback<Vec<u8>>>,
    #[prop(optional)] on_resize: Option<Callback<(u16, u16)>>,
) -> impl IntoView {
    let cols_sig = cols.unwrap_or_else(|| RwSignal::new(80));
    let rows_sig = rows.unwrap_or_else(|| RwSignal::new(24));

    view! {
        <div
            class="terminal-pane"
            data-status=move || status.get().as_str()
            data-testid="terminal"
        >
            <header class="terminal-head">
                <span class="terminal-title">{move || title.get()}</span>
                <span class="terminal-status" data-status=move || status.get().as_str()>
                    {move || status.get().as_str()}
                </span>
                <div class="terminal-size-controls">
                    <span class="muted small">{move || format!("{}x{}", cols_sig.get(), rows_sig.get())}</span>
                    <button
                        type="button"
                        class="button small"
                        data-testid="terminal-resize-wider"
                        on:click=move |_| {
                            let next_c = (cols_sig.get_untracked() + 20).min(240);
                            let r = rows_sig.get_untracked();
                            cols_sig.set(next_c);
                            if let Some(cb) = on_resize {
                                cb.run((next_c, r));
                            }
                        }
                    >"+Cols"</button>
                    <button
                        type="button"
                        class="button small"
                        data-testid="terminal-resize-taller"
                        on:click=move |_| {
                            let c = cols_sig.get_untracked();
                            let next_r = (rows_sig.get_untracked() + 6).min(120);
                            rows_sig.set(next_r);
                            if let Some(cb) = on_resize {
                                cb.run((c, next_r));
                            }
                        }
                    >"+Rows"</button>
                </div>
            </header>
            {move || tabs.map(|tabs_sig| {
                let list = tabs_sig.get();
                let current = active_id.and_then(|s| s.get());
                view! {
                    <div class="terminal-tabs" role="tablist" data-testid="terminal-tabs">
                        {list.into_iter().map(|tab| {
                            let id_sel = tab.id.clone();
                            let id_cls = tab.id.clone();
                            let is_active = current.as_deref() == Some(tab.id.as_str());
                            let label = if tab.title.is_empty() {
                                tab.id.clone()
                            } else {
                                tab.title.clone()
                            };
                            view! {
                                <div class="terminal-tab" data-active=is_active.to_string()>
                                    <button
                                        type="button"
                                        role="tab"
                                        aria-selected=is_active.to_string()
                                        on:click=move |_| {
                                            if let Some(cb) = on_select_tab {
                                                cb.run(id_sel.clone());
                                            }
                                        }
                                    >{label}</button>
                                    <button
                                        type="button"
                                        aria-label="Close terminal tab"
                                        on:click=move |_| {
                                            if let Some(cb) = on_close_tab {
                                                cb.run(id_cls.clone());
                                            }
                                        }
                                    >"×"</button>
                                </div>
                            }
                        }).collect_view()}
                        {on_new_tab.map(|cb| view! {
                            <button
                                type="button"
                                class="terminal-new-tab"
                                data-testid="terminal-new-tab"
                                on:click=move |_| cb.run(())
                            >"+ Tab"</button>
                        })}
                    </div>
                }
            })}
            <div
                class="terminal-screen"
                tabindex="0"
                role="textbox"
                aria-label="Terminal input"
                data-testid="terminal-screen"
                on:keydown=move |ev| {
                    if let Some(bytes) = encode_terminal_key(&ev.key(), ev.ctrl_key(), ev.alt_key(), ev.shift_key()) {
                        ev.prevent_default();
                        if let Some(cb) = on_key_input {
                            cb.run(bytes);
                        }
                    }
                }
            >
                <pre class="terminal-output" role="log">
                    <For
                        each={move || lines.get().into_iter().enumerate().collect::<Vec<_>>()}
                        key=|(idx, line)| (*idx, line.clone())
                        children={move |(_, line): (usize, String)| view! {
                            <span class="terminal-line" inner_html=line></span>
                        }}
                    />
                </pre>
            </div>
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
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Listening => "listening",
            Self::Processing => "processing",
            Self::Speaking => "speaking",
        }
    }
}

/// Voice controls: WebAudio mic toggle, VAD pause slider, engine/voice selector, sample playback, and transcript.
#[component]
pub fn Voice(
    state: RwSignal<VoiceState>,
    on_toggle: Callback<()>,
    #[prop(optional)] view_info: Option<Signal<VoiceView>>,
    #[prop(optional)] mic_muted: Option<RwSignal<bool>>,
    #[prop(optional)] pause_secs: Option<RwSignal<f64>>,
    #[prop(optional)] interim_text: Option<Signal<String>>,
    #[prop(optional)] final_text: Option<Signal<String>>,
    #[prop(optional)] on_choose_voice: Option<Callback<(String, String, f64)>>,
    #[prop(optional)] on_set_vad: Option<Callback<f64>>,
    #[prop(optional)] on_test_sample: Option<Callback<()>>,
    #[prop(optional)] on_cancel_tts: Option<Callback<()>>,
) -> impl IntoView {
    let muted = mic_muted.unwrap_or_else(|| RwSignal::new(false));
    let pause = pause_secs.unwrap_or_else(|| RwSignal::new(DEFAULT_PAUSE_SECS));
    let custom_engine = RwSignal::new(String::new());
    let custom_voice = RwSignal::new(String::new());

    view! {
        <div class="voice-controls" data-state=move || state.get().as_str() data-testid="voice-controls">
            <div class="voice-header-row">
                <span class="voice-status" aria-live="polite">{move || state.get().as_str()}</span>
                <button type="button" class="voice-toggle" data-testid="voice-toggle-btn" aria-label="Toggle voice" on:click=move |_| on_toggle.run(())>
                    {move || if state.get() == VoiceState::Idle { "Start Voice" } else { "Stop Voice" }}
                </button>
                <button
                    type="button"
                    class="button small"
                    data-testid="voice-mute-btn"
                    on:click=move |_| muted.update(|m| *m = !*m)
                >
                    {move || if muted.get() { "Unmute Mic" } else { "Mute Mic" }}
                </button>
                {on_cancel_tts.map(|cb| view! {
                    <button
                        type="button"
                        class="button small"
                        data-testid="voice-cancel-btn"
                        on:click=move |_| cb.run(())
                    >"Interrupt"</button>
                })}
            </div>

            <label class="voice-setting-row">
                <span class="small muted">{move || format!("Pause before sending: {:.1}s", pause.get())}</span>
                <input
                    type="range"
                    min=MIN_PAUSE_SECS.to_string()
                    max=MAX_PAUSE_SECS.to_string()
                    step="0.1"
                    prop:value=move || pause.get().to_string()
                    on:input=move |ev| {
                        if let Ok(val) = event_target_value(&ev).parse::<f64>() {
                            let clamped = val.clamp(MIN_PAUSE_SECS, MAX_PAUSE_SECS);
                            pause.set(clamped);
                            if let Some(cb) = on_set_vad {
                                cb.run(clamped);
                            }
                        }
                    }
                />
            </label>

            {move || view_info.map(|sig| {
                let v = sig.get();
                let engines = v.available_engines.clone();
                view! {
                    <div class="voice-engine-picker">
                        <div class="small muted">
                            {format!(
                                "Engine: {} · Voice: {}",
                                v.engine.as_deref().unwrap_or("default"),
                                v.voice.as_deref().unwrap_or("default")
                            )}
                        </div>
                        {(!engines.is_empty()).then(|| view! {
                            <select
                                on:change=move |ev| custom_engine.set(event_target_value(&ev))
                            >
                                <option value="">"-- select engine --"</option>
                                {engines.into_iter().map(|eng| {
                                    let val = eng.clone();
                                    view! {
                                        <option value=val>{eng}</option>
                                    }
                                }).collect_view()}
                            </select>
                        })}
                        <input
                            type="text"
                            placeholder="Voice ID"
                            prop:value=move || custom_voice.get()
                            on:input=move |ev| custom_voice.set(event_target_value(&ev))
                        />
                        <div class="voice-action-row">
                            {on_choose_voice.map(|cb| view! {
                                <button
                                    type="button"
                                    class="button small"
                                    on:click=move |_| {
                                        cb.run((
                                            custom_engine.get_untracked(),
                                            custom_voice.get_untracked(),
                                            1.0,
                                        ));
                                    }
                                >"Apply Voice"</button>
                            })}
                            {on_test_sample.map(|cb| view! {
                                <button
                                    type="button"
                                    class="button small"
                                    data-testid="voice-sample-btn"
                                    on:click=move |_| cb.run(())
                                >"Play Sample"</button>
                            })}
                        </div>
                    </div>
                }
            })}

            {move || interim_text.map(|s| {
                let txt = s.get();
                (!txt.is_empty()).then(|| view! {
                    <p class="voice-interim muted small" data-testid="voice-interim">{txt}</p>
                })
            })}
            {move || final_text.map(|s| {
                let txt = s.get();
                (!txt.is_empty()).then(|| view! {
                    <p class="voice-final small" data-testid="voice-final">{txt}</p>
                })
            })}
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
