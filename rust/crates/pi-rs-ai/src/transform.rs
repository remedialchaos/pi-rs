use std::collections::{HashMap, HashSet};

use crate::model::Model;
use crate::types::{
    AssistantContent, AssistantMessage, ImageContent, InputContent, Message, StringOrBlocks,
    TextContent, ToolResultMessage, unix_millis,
};

const USER_IMAGE_OMITTED: &str = "(image omitted: model does not support images)";
const TOOL_IMAGE_OMITTED: &str = "(tool image omitted: model does not support images)";
const ERROR_TOOL_IMAGE: &str = "(image attached after the tool results)";

pub struct SplitErrorToolResult {
    pub content: Vec<TextContent>,
    pub caption: String,
    pub images: Vec<ImageContent>,
}

fn replace_images(
    blocks: impl IntoIterator<Item = InputContent>,
    placeholder: &str,
) -> Vec<TextContent> {
    let mut output = Vec::<TextContent>::new();
    for block in blocks {
        match block {
            InputContent::Text(text) => output.push(text),
            InputContent::Image(_) if output.last().is_none_or(|part| part.text != placeholder) => {
                output.push(placeholder.into())
            }
            _ => {}
        }
    }
    output
}

pub fn split_error_tool_result_images(message: &ToolResultMessage) -> Option<SplitErrorToolResult> {
    if !message.is_error {
        return None;
    }
    let images: Vec<_> = message
        .content
        .iter()
        .filter_map(|block| match block {
            InputContent::Image(image) => Some(image.clone()),
            _ => None,
        })
        .collect();
    if images.is_empty() {
        return None;
    }
    let content = replace_images(
        message.content.iter().map(|block| match block {
            InputContent::Image(_) => InputContent::Image(ImageContent {
                kind: Default::default(),
                data: String::new(),
                mime_type: String::new(),
            }),
            InputContent::Text(text) => InputContent::Text(text.clone()),
        }),
        ERROR_TOOL_IMAGE,
    );
    let caption = format!(
        "Images from the error result of tool call {} ({}):",
        message.tool_call_id, message.tool_name
    );
    Some(SplitErrorToolResult {
        content,
        caption,
        images,
    })
}

pub fn transform_messages(
    messages: Vec<Message>,
    model: &Model,
    mut normalize: impl FnMut(&str, &Model, &AssistantMessage) -> Option<String>,
) -> Vec<Message> {
    let vision = model.supports_images();
    let mut id_map = HashMap::new();
    let mut output = Vec::with_capacity(messages.len());
    let mut pending = Vec::new();
    let mut resolved = HashSet::new();
    let mut held = Vec::new();
    let close = |output: &mut Vec<Message>,
                 pending: &mut Vec<(String, String)>,
                 resolved: &mut HashSet<String>,
                 held: &mut Vec<Message>| {
        for (id, name) in pending.drain(..) {
            if !resolved.contains(&id) {
                output.push(Message::ToolResult(ToolResultMessage {
                    role: Default::default(),
                    tool_call_id: id,
                    tool_name: name,
                    content: vec![InputContent::Text("No result provided".into())],
                    details: None,
                    usage: None,
                    nested_calls: None,
                    is_error: true,
                    timestamp: unix_millis(),
                    duration_ms: None,
                }));
            }
        }
        resolved.clear();
        output.append(held);
    };
    for message in messages {
        match message {
            Message::Assistant(mut a) => {
                close(&mut output, &mut pending, &mut resolved, &mut held);
                let same =
                    a.model == model.id && a.provider == model.provider && a.api == model.api;
                let mut content = std::mem::take(&mut a.content);
                content.retain_mut(|block| match block {
                    AssistantContent::Thinking(t) => {
                        same && (t.redacted == Some(true)
                            || t.thinking_signature.as_ref().is_some_and(|s| !s.is_empty())
                            || !t.thinking.trim().is_empty())
                    }
                    AssistantContent::Text(t) => {
                        if !same {
                            t.text_signature = None;
                        }
                        true
                    }
                    AssistantContent::ToolCall(t) => {
                        if !same {
                            t.thought_signature = None;
                            if let Some(id) = normalize(&t.id, model, &a)
                                && id != t.id
                            {
                                id_map.insert(std::mem::replace(&mut t.id, id.clone()), id);
                            }
                        }
                        true
                    }
                });
                if a.stop_reason.failed() {
                    continue;
                }
                for tool in content.iter().filter_map(|b| match b {
                    AssistantContent::ToolCall(t) => Some(t),
                    _ => None,
                }) {
                    pending.push((tool.id.clone(), tool.name.clone()));
                }
                a.content = content;
                output.push(Message::Assistant(a));
            }
            Message::User(mut u) => {
                close(&mut output, &mut pending, &mut resolved, &mut held);
                if !vision && let StringOrBlocks::Blocks(parts) = u.content {
                    u.content = StringOrBlocks::Blocks(
                        replace_images(parts, USER_IMAGE_OMITTED)
                            .into_iter()
                            .map(InputContent::Text)
                            .collect(),
                    );
                }
                output.push(Message::User(u));
            }
            Message::ToolResult(mut tool) => {
                if let Some(new) = id_map.get(&tool.tool_call_id).filter(|id| !id.is_empty()) {
                    tool.tool_call_id.clone_from(new);
                }
                if !vision {
                    tool.content = replace_images(tool.content, TOOL_IMAGE_OMITTED)
                        .into_iter()
                        .map(InputContent::Text)
                        .collect();
                }
                if !pending.is_empty() {
                    resolved.insert(tool.tool_call_id.clone());
                }
                output.push(Message::ToolResult(tool));
            }
            Message::System(system) if !pending.is_empty() => held.push(Message::System(system)),
            other => output.push(other),
        }
    }
    close(&mut output, &mut pending, &mut resolved, &mut held);
    output
}
