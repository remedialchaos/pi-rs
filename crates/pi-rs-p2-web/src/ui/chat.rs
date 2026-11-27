//! Streaming chat transcript and code blocks (upstream `Chat.tsx`, `Code.tsx`).

use crate::activity::model::default_tool_activities;
use crate::browser_plugins::ChatActivity;
use crate::markdown::{highlight_code, render_html};
use crate::state::virtual_list::{DEFAULT_ROW_HEIGHT_PX, MIN_OVERSCAN_PX, RowLayout};
use crate::ui::activity_ui::ActivityRow;
use leptos::prelude::*;

/// One message in the transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatMessage {
    pub id: String,
    pub role: MessageRole,
    pub text: String,
    /// Still receiving tokens.
    pub streaming: bool,
    /// Tool/activity cards associated with this turn.
    pub activities: Vec<ChatActivity>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
    Tool,
}

impl MessageRole {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

/// Appends a streamed token to the assistant message with `id`, creating it when needed.
/// Returns whether a message changed (the fine-grained UI updates only that row).
pub fn append_stream_token(messages: &mut Vec<ChatMessage>, id: &str, token: &str) -> bool {
    if let Some(last) = messages.last_mut()
        && last.id == id
        && last.role == MessageRole::Assistant
    {
        last.text.push_str(token);
        return true;
    }
    messages.push(ChatMessage {
        id: id.to_owned(),
        role: MessageRole::Assistant,
        text: token.to_owned(),
        streaming: true,
        activities: Vec::new(),
    });
    true
}

/// Marks the message with `id` complete (upstream stops the pulsing dot when streaming ends).
pub fn finish_stream(messages: &mut [ChatMessage], id: &str) -> bool {
    if let Some(message) = messages.iter_mut().find(|message| message.id == id) {
        message.streaming = false;
        return true;
    }
    false
}

/// A syntax-highlighted code block (upstream `Code.tsx`).
#[component]
pub fn Code(code: String, #[prop(optional)] language: Option<String>) -> impl IntoView {
    let html = highlight_code(&code, language.as_deref());
    view! {
        <pre class="code-block" data-language=language.clone().unwrap_or_default()>
            <code inner_html=html></code>
        </pre>
    }
}

/// One transcript row. Assistant text is rendered as Elanor Markdown; user text is escaped.
#[component]
pub fn MessageRow(message: ChatMessage) -> impl IntoView {
    let is_assistant = message.role == MessageRole::Assistant;
    let is_tool = message.role == MessageRole::Tool;
    let role = message.role.as_str();
    let html = if is_assistant {
        render_html(&message.text)
    } else {
        String::new()
    };
    let text = message.text.clone();
    let activities = message.activities.clone();
    let tools = default_tool_activities();
    view! {
        <div class="chat-row" data-role=role data-streaming=message.streaming.to_string() data-entry-id=message.id.clone()>
            {(!text.is_empty() && !is_tool).then(|| {
                if is_assistant {
                    view! { <div class="elanor-markdown" inner_html=html.clone()></div> }.into_any()
                } else {
                    view! { <div class="user-message">{text.clone()}</div> }.into_any()
                }
            })}
            {(!activities.is_empty()).then(|| {
                let activities = activities.clone();
                let tools = tools.clone();
                view! {
                    <div class="chat-activities" data-testid="chat-activities">
                        {activities.into_iter().map(|act| {
                            view! { <ActivityRow step=act.step described=act.described tools=tools.clone() /> }
                        }).collect_view()}
                    </div>
                }
            })}
        </div>
    }
}

/// Threshold above which transcript windowing renders only visible rows + spacers.
const VIRTUALIZE_THRESHOLD: usize = 80;

/// The transcript: a scroll container of reactive message rows. Streaming token appends
/// mutate only the last row through the signal, so a long transcript is never re-diffed.
#[component]
pub fn Chat(messages: RwSignal<Vec<ChatMessage>>) -> impl IntoView {
    let scroll_top = RwSignal::new(0.0f64);
    let viewport_h = RwSignal::new(720.0f64);

    let windowed = Memo::new(move |_| {
        let all = messages.get();
        if all.len() <= VIRTUALIZE_THRESHOLD {
            return (0.0f64, 0.0f64, all);
        }
        let mut layout = RowLayout::new(DEFAULT_ROW_HEIGHT_PX);
        layout.set_keys(all.iter().map(|m| m.id.clone()).collect());
        let win = layout.window(scroll_top.get(), viewport_h.get(), MIN_OVERSCAN_PX);
        let slice = all[win.start..win.end].to_vec();
        (win.top_spacer_px, win.bottom_spacer_px, slice)
    });

    view! {
        <div
            class="chat"
            data-testid="chat"
            on:scroll=move |ev| {
                #[cfg(target_arch = "wasm32")]
                {
                    use wasm_bindgen::JsCast;
                    if let Some(target) = ev.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) {
                        scroll_top.set(f64::from(target.scroll_top()));
                        viewport_h.set(f64::from(target.client_height()));
                    }
                }
                #[cfg(not(target_arch = "wasm32"))]
                let _ = ev;
            }
        >
            <div class="chat-transcript" role="log" aria-live="polite">
                <div class="virtual-spacer-top" style=move || format!("height: {}px", windowed.get().0)></div>
                <For
                    each=move || windowed.get().2
                    key=|message| (message.id.clone(), message.text.clone(), message.streaming, message.activities.len())
                    children={move |message: ChatMessage| view! { <MessageRow message=message /> }}
                />
                <div class="virtual-spacer-bottom" style=move || format!("height: {}px", windowed.get().1)></div>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str, text: &str, role: MessageRole, streaming: bool) -> ChatMessage {
        ChatMessage {
            id: id.into(),
            role,
            text: text.into(),
            streaming,
            activities: Vec::new(),
        }
    }

    #[test]
    fn streaming_appends_to_the_last_assistant_row() {
        let mut messages = vec![message("1", "hi", MessageRole::User, false)];
        assert!(append_stream_token(&mut messages, "1", "Hello "));
        assert!(append_stream_token(&mut messages, "1", "world"));
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].text, "Hello world");
        assert!(messages[1].streaming);
    }

    #[test]
    fn finishing_a_stream_clears_the_flag() {
        let mut messages = vec![message("1", "done", MessageRole::Assistant, true)];
        assert!(finish_stream(&mut messages, "1"));
        assert!(!messages[0].streaming);
        assert!(!finish_stream(&mut messages, "missing"));
    }
}
