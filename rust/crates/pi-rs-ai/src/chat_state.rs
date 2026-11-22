use std::collections::HashMap;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::event_stream::{
    AssistantMessageEventSender, AssistantMessageEventStream, live, read_partial, write_partial,
};
use crate::model::Model;
use crate::streaming_json::ToolCallArgumentsStream;
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent as Event, PartialMessage, StopReason,
    TextContent, ThinkingContent, ToolCall,
};

pub(crate) struct ChatState {
    events: AssistantMessageEventSender,
    pub message: PartialMessage,
    tools: HashMap<usize, ToolCallArgumentsStream>,
    active: Vec<bool>,
    started: bool,
}
impl ChatState {
    pub fn new(model: &Model) -> (Self, AssistantMessageEventStream) {
        let (events, stream) = AssistantMessageEventStream::channel();
        let message = live(AssistantMessage::pending(model));
        (
            Self {
                events,
                message,
                tools: HashMap::new(),
                active: Vec::new(),
                started: false,
            },
            stream,
        )
    }
    pub fn start(&mut self) {
        if !self.started {
            self.started = true;
            self.events.send(Event::Start {
                partial: self.message.clone(),
            });
        }
    }
    pub fn started(&self) -> bool {
        self.started
    }
    fn push(&mut self, content: AssistantContent) -> usize {
        self.start();
        let mut msg = write_partial(&self.message);
        let index = msg.content.len();
        msg.content.push(content);
        self.active.push(true);
        index
    }
    pub fn text_start(&mut self, signature: Option<String>) -> usize {
        let index = self.push(AssistantContent::Text(TextContent {
            text_signature: signature,
            ..TextContent::default()
        }));
        self.events.send(Event::TextStart {
            content_index: index,
            partial: self.message.clone(),
        });
        index
    }
    pub fn thinking_start(&mut self, thinking: ThinkingContent) -> usize {
        let index = self.push(AssistantContent::Thinking(thinking));
        self.events.send(Event::ThinkingStart {
            content_index: index,
            partial: self.message.clone(),
        });
        index
    }
    pub fn tool_start(&mut self, call: ToolCall) -> usize {
        let index = self.push(AssistantContent::ToolCall(call));
        self.events.send(Event::ToolCallStart {
            content_index: index,
            partial: self.message.clone(),
        });
        index
    }
    pub fn delta(&mut self, index: usize, delta: String) -> Result<()> {
        if delta.is_empty() {
            return Ok(());
        }
        let mut message = write_partial(&self.message);
        let Some(content) = message.content.get_mut(index) else {
            return Err(Error::Protocol(format!("Unknown content block {index}")));
        };
        let partial = self.message.clone();
        let event = match content {
            AssistantContent::Text(t) => {
                t.text.push_str(&delta);
                Event::TextDelta {
                    content_index: index,
                    delta,
                    partial,
                }
            }
            AssistantContent::Thinking(t) => {
                t.thinking.push_str(&delta);
                Event::ThinkingDelta {
                    content_index: index,
                    delta,
                    partial,
                }
            }
            AssistantContent::ToolCall(t) => {
                let parser = self.tools.entry(index).or_default();
                if let Some(args) = parser.append(&delta).and_then(|v| {
                    if Arc::strong_count(&v) == 1 {
                        match Arc::unwrap_or_clone(v) {
                            serde_json::Value::Object(v) => Some(v),
                            _ => None,
                        }
                    } else {
                        v.as_object().cloned()
                    }
                }) {
                    t.arguments = args;
                }
                Event::ToolCallDelta {
                    content_index: index,
                    delta,
                    partial,
                }
            }
        };
        drop(message);
        self.events.send(event);
        Ok(())
    }
    pub fn replace_tool_args(&mut self, index: usize, text: &str) -> Result<()> {
        let parser = self.tools.entry(index).or_default();
        let (delta, value) = parser.replace(text);
        if let Some(AssistantContent::ToolCall(call)) =
            write_partial(&self.message).content.get_mut(index)
        {
            call.arguments = value
                .as_deref()
                .and_then(serde_json::Value::as_object)
                .cloned()
                .unwrap_or_default();
        } else {
            return Err(Error::Protocol(format!("Unknown tool call block {index}")));
        }
        if let Some(delta) = delta.filter(|d| !d.is_empty()) {
            self.events.send(Event::ToolCallDelta {
                content_index: index,
                delta,
                partial: self.message.clone(),
            });
        }
        Ok(())
    }
    pub fn tool_update(&self, index: usize, update: impl FnOnce(&mut ToolCall)) {
        if let Some(AssistantContent::ToolCall(call)) =
            write_partial(&self.message).content.get_mut(index)
        {
            update(call);
        }
    }
    pub fn thinking_update(&self, index: usize, update: impl FnOnce(&mut ThinkingContent)) {
        if let Some(AssistantContent::Thinking(part)) =
            write_partial(&self.message).content.get_mut(index)
        {
            update(part);
        }
    }
    pub fn text_update(&self, index: usize, update: impl FnOnce(&mut TextContent)) {
        if let Some(AssistantContent::Text(part)) =
            write_partial(&self.message).content.get_mut(index)
        {
            update(part);
        }
    }
    pub fn end(&mut self, index: usize) {
        let Some(active) = self.active.get_mut(index) else {
            return;
        };
        if !std::mem::replace(active, false) {
            return;
        }
        let mut message = write_partial(&self.message);
        let Some(content) = message.content.get_mut(index) else {
            return;
        };
        let partial = self.message.clone();
        let event = match content {
            AssistantContent::Text(t) => Event::TextEnd {
                content_index: index,
                content: t.text.clone(),
                partial,
            },
            AssistantContent::Thinking(t) => Event::ThinkingEnd {
                content_index: index,
                content: t.thinking.clone(),
                partial,
            },
            AssistantContent::ToolCall(t) => {
                if let Some(parser) = self.tools.remove(&index) {
                    (t.arguments, t.arguments_error) = parser.finish();
                }
                Event::ToolCallEnd {
                    content_index: index,
                    tool_call: t.clone(),
                    partial,
                }
            }
        };
        drop(message);
        self.events.send(event);
    }
    pub fn finish(mut self, result: Result<StopReason>) {
        if result.as_ref().is_ok_and(|reason| !reason.failed()) {
            self.start();
            for index in 0..self.active.len() {
                self.end(index);
            }
        } else {
            // Raw partial arguments never persist in the transcript; finalize the objects even when a wire fault interrupts a tool call.
            let mut message = write_partial(&self.message);
            for (index, parser) in self.tools.drain() {
                if let Some(AssistantContent::ToolCall(t)) = message.content.get_mut(index) {
                    (t.arguments, t.arguments_error) = parser.finish();
                }
            }
        }
        let final_msg = {
            let mut guard = write_partial(&self.message);
            match result {
                Ok(reason) => guard.stop_reason = reason,
                Err(error) => {
                    guard.stop_reason = if matches!(error, Error::Aborted) {
                        StopReason::Aborted
                    } else {
                        StopReason::Error
                    };
                    guard.error_message = Some(error.to_string());
                }
            }
            guard.clone()
        };
        self.events.finish(final_msg);
    }
    pub fn has_tool_calls(&self) -> bool {
        read_partial(&self.message)
            .content
            .iter()
            .any(|b| matches!(b, AssistantContent::ToolCall(_)))
    }
}
