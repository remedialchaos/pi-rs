//! Streaming chat transcript and code blocks (upstream `Chat.tsx`, `Code.tsx`).

use crate::markdown::{highlight_code, render_html};
use leptos::prelude::*;

/// One message in the transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatMessage {
    pub id: String,
    pub role: MessageRole,
    pub text: String,
    /// Still receiving tokens.
    pub streaming: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
}

impl MessageRole {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
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
    let role = message.role.as_str();
    let html = if is_assistant {
        render_html(&message.text)
    } else {
        String::new()
    };
    let text = message.text.clone();
    view! {
        <div class="chat-row" data-role=role data-streaming=message.streaming.to_string() data-entry-id=message.id.clone()>
            {move || if is_assistant {
                view! { <div class="elanor-markdown" inner_html=html.clone()></div> }.into_any()
            } else {
                view! { <div class="user-message">{text.clone()}</div> }.into_any()
            }}
        </div>
    }
}

/// The transcript: a scroll container of reactive message rows. Streaming token appends
/// mutate only the last row through the signal, so a long transcript is never re-diffed.
#[component]
pub fn Chat(messages: RwSignal<Vec<ChatMessage>>) -> impl IntoView {
    view! {
        <div class="chat" data-testid="chat">
            <div class="chat-transcript" role="log" aria-live="polite">
                <For
                    each=move || messages.get()
                    key=|message| message.id.clone()
                    children={move |message: ChatMessage| view! { <MessageRow message=message /> }}
                />
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
