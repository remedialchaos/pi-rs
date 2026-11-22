use pi_rs_agent::AgentEvent;
use pi_rs_ai::{AssistantContent, AssistantMessageEvent};
use serde_json::{Value, json};

use crate::core::agent_session::AgentSessionEvent;

/// Emit streaming deltas without serializing the cumulative assistant snapshot.
pub fn to_json_event(event: &AgentSessionEvent) -> serde_json::Result<Value> {
    let AgentSessionEvent::Core(AgentEvent::MessageUpdate {
        message,
        assistant_message_event: delta,
    }) = event
    else {
        return serde_json::to_value(event);
    };
    use AssistantMessageEvent::*;
    let delta = match delta {
        Start { .. } => json!({"type": "start"}),
        TextStart { content_index, .. } => {
            json!({"type": "text_start", "contentIndex": content_index})
        }
        TextDelta {
            content_index,
            delta,
            ..
        } => json!({"type": "text_delta", "contentIndex": content_index, "delta": delta}),
        TextEnd {
            content_index,
            content,
            ..
        } => json!({"type": "text_end", "contentIndex": content_index, "content": content}),
        ThinkingStart { content_index, .. } => {
            json!({"type": "thinking_start", "contentIndex": content_index})
        }
        ThinkingDelta {
            content_index,
            delta,
            ..
        } => json!({"type": "thinking_delta", "contentIndex": content_index, "delta": delta}),
        ThinkingEnd {
            content_index,
            content,
            ..
        } => json!({"type": "thinking_end", "contentIndex": content_index, "content": content}),
        ToolCallStart {
            content_index,
            partial,
        } => {
            let partial = partial.read().unwrap_or_else(|error| error.into_inner());
            let Some(AssistantContent::ToolCall(call)) = partial.content.get(*content_index) else {
                return Err(serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("toolcall_start content at index {content_index} is not a tool call"),
                )));
            };
            json!({"type": "toolcall_start", "contentIndex": content_index, "id": call.id, "toolName": call.name})
        }
        ToolCallDelta {
            content_index,
            delta,
            ..
        } => json!({"type": "toolcall_delta", "contentIndex": content_index, "delta": delta}),
        ToolCallEnd {
            content_index,
            tool_call,
            ..
        } => json!({"type": "toolcall_end", "contentIndex": content_index, "toolCall": tool_call}),
        Done { reason, message } => json!({"type": "done", "reason": reason, "message": message}),
        Error { reason, error } => json!({"type": "error", "reason": reason, "error": error}),
    };
    let message = message.read().unwrap_or_else(|error| error.into_inner());
    Ok(json!({"type": "message_update", "usage": message.usage, "assistantMessageEvent": delta}))
}
