use std::borrow::Cow;
use std::collections::HashSet;

use indexmap::IndexMap;

use crate::types::{
    Context, InputContent, Message, StringOrBlocks, SystemMessage, Tool, ToolReference,
    TranscriptContext,
};

pub fn normalize_context(mut context: Context) -> TranscriptContext {
    if context
        .system_prompt
        .as_ref()
        .is_some_and(|p| !p.is_empty())
        || context.tools.as_ref().is_some_and(|t| !t.is_empty())
    {
        context.messages.insert(
            0,
            Message::System(SystemMessage {
                content: StringOrBlocks::String(context.system_prompt.unwrap_or_default()),
                tools_added: context.tools.filter(|tools| !tools.is_empty()),
                ..SystemMessage::default()
            }),
        );
    }
    TranscriptContext {
        messages: context.messages,
    }
}

pub fn get_initial_system_message(messages: &[Message]) -> Option<&SystemMessage> {
    match messages.first()? {
        Message::System(m) => Some(m),
        _ => None,
    }
}
pub fn without_initial_system_message(messages: &[Message]) -> &[Message] {
    if get_initial_system_message(messages).is_some() {
        &messages[1..]
    } else {
        messages
    }
}
fn systems(messages: &[Message]) -> impl Iterator<Item = &SystemMessage> {
    messages.iter().filter_map(|m| match m {
        Message::System(m) => Some(m),
        _ => None,
    })
}
fn system_content(message: &SystemMessage) -> Cow<'_, str> {
    match &message.content {
        StringOrBlocks::String(s) => Cow::Borrowed(s),
        StringOrBlocks::Blocks(blocks) => {
            Cow::Owned(join(blocks.iter().map(|b| b.text.as_str()), "\n"))
        }
    }
}
fn join<'a>(pieces: impl IntoIterator<Item = &'a str>, delimiter: &str) -> String {
    let mut pieces = pieces.into_iter();
    let mut out = pieces.next().unwrap_or_default().to_owned();
    for piece in pieces {
        out.push_str(delimiter);
        out.push_str(piece);
    }
    out
}
pub fn input_text(content: &[InputContent], sep: &str) -> String {
    join(
        content.iter().filter_map(|b| match b {
            InputContent::Text(b) => Some(b.text.as_str()),
            _ => None,
        }),
        sep,
    )
}
pub fn system_message_text(message: &SystemMessage) -> String {
    let text = system_content(message);
    join(
        std::iter::once(text.as_ref())
            .chain(
                message
                    .sections
                    .iter()
                    .flat_map(|s| s.values().filter_map(Option::as_deref)),
            )
            .filter(|text| !text.is_empty()),
        "\n\n",
    )
}
pub fn render_system_message_update(message: &SystemMessage) -> String {
    use std::fmt::Write as _;
    let mut text = system_content(message).into_owned();
    for (name, value) in message.sections.iter().flatten() {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        match value {
            None => {
                let _ = write!(text, "Removed system prompt section \"{name}\".");
            }
            Some(val) => {
                let _ = write!(text, "Updated system prompt section \"{name}\":\n\n{val}");
            }
        }
    }
    text
}
fn apply_tools<'a>(tools: &mut IndexMap<&'a str, &'a Tool>, message: &'a SystemMessage) {
    for old in message.tools_removed.iter().flatten() {
        tools.shift_remove(old.name.as_str());
    }
    for tool in message.tools_added.iter().flatten() {
        tools.insert(tool.name.as_str(), tool);
    }
}
pub fn get_current_tools(messages: &[Message]) -> Vec<&Tool> {
    let mut tools = IndexMap::new();
    for message in systems(messages) {
        apply_tools(&mut tools, message);
    }
    tools.into_values().collect()
}
pub fn get_current_system_message(messages: &[Message]) -> Option<SystemMessage> {
    let mut timestamp = None;
    let mut text = String::new();
    let mut sections = IndexMap::new();
    let mut tools = IndexMap::new();
    for message in systems(messages) {
        timestamp.get_or_insert(message.timestamp);
        let content = system_content(message);
        if !content.is_empty() {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&content);
        }
        for (name, value) in message.sections.iter().flatten() {
            if let Some(value) = value {
                sections.insert(name, value);
            } else {
                sections.shift_remove(name);
            }
        }
        apply_tools(&mut tools, message);
    }
    Some(SystemMessage {
        content: StringOrBlocks::String(text),
        sections: (!sections.is_empty()).then(|| {
            sections
                .into_iter()
                .map(|(k, v)| (k.clone(), Some(v.clone())))
                .collect()
        }),
        tools_added: (!tools.is_empty()).then(|| tools.into_values().cloned().collect()),
        timestamp: timestamp?,
        ..SystemMessage::default()
    })
}
pub fn get_current_system_prompt(messages: &[Message]) -> String {
    get_current_system_message(messages).map_or_else(String::new, |m| system_message_text(&m))
}
pub fn collapse_system_messages(mut context: TranscriptContext) -> TranscriptContext {
    let head = get_current_system_message(&context.messages);
    context
        .messages
        .retain(|m| !matches!(m, Message::System(_)));
    if let Some(head) = head {
        context.messages.insert(0, Message::System(head));
    }
    context
}
pub fn resolve_transcript(
    context: TranscriptContext,
    supports_mid_convo_system_messages: bool,
) -> TranscriptContext {
    if supports_mid_convo_system_messages {
        context
    } else {
        collapse_system_messages(context)
    }
}
pub fn declarations_equal(left: &Tool, right: &Tool) -> bool {
    // Schema object-key order is significant for provider replay and prefix caching.
    left.name == right.name
        && left.description == right.description
        && serde_json::to_string(&left.parameters).ok()
            == serde_json::to_string(&right.parameters).ok()
        && serde_json::to_string(&left.constrained_sampling).ok()
            == serde_json::to_string(&right.constrained_sampling).ok()
}
#[derive(Debug, Default, PartialEq)]
pub struct ToolStateChanges {
    pub tools_added: Vec<Tool>,
    pub tools_removed: Vec<ToolReference>,
}
pub fn get_tool_state_changes(previous: &[Tool], current: &[Tool]) -> ToolStateChanges {
    let index = |items: &[Tool]| {
        items
            .iter()
            .map(|t| (t.name.clone(), t.clone()))
            .collect::<IndexMap<_, _>>()
    };
    let previous_index = index(previous);
    let current_index = index(current);
    ToolStateChanges {
        tools_added: current
            .iter()
            .filter(|t| {
                previous_index
                    .get(&t.name)
                    .is_none_or(|p| !declarations_equal(t, p))
            })
            .cloned()
            .collect(),
        tools_removed: previous
            .iter()
            .filter(|t| {
                current_index
                    .get(&t.name)
                    .is_none_or(|c| !declarations_equal(t, c))
            })
            .map(|t| ToolReference {
                name: t.name.clone(),
            })
            .collect(),
    }
}
pub fn get_declared_tools(messages: &[Message]) -> Vec<&Tool> {
    let mut tools = IndexMap::new();
    for tool in systems(messages).flat_map(|m| m.tools_added.iter().flatten()) {
        tools.insert(tool.name.as_str(), tool);
    }
    tools.into_values().collect()
}
pub fn has_tool_redefinitions(messages: &[Message]) -> bool {
    let mut tools = IndexMap::new();
    systems(messages)
        .flat_map(|m| m.tools_added.iter().flatten())
        .any(|tool| {
            tools
                .insert(tool.name.as_str(), tool)
                .is_some_and(|old| !declarations_equal(old, tool))
        })
}
fn last_non_additive_tool_change(messages: &[Message]) -> Option<usize> {
    let mut declared = HashSet::new();
    let mut last = None;
    for (i, message) in systems(messages).enumerate() {
        if message
            .tools_removed
            .as_ref()
            .is_some_and(|tools| !tools.is_empty())
        {
            last = Some(i);
        }
        for tool in message.tools_added.iter().flatten() {
            if !declared.insert(tool.name.as_str()) {
                last = Some(i);
            }
        }
    }
    last
}
pub fn has_non_additive_tool_changes(messages: &[Message]) -> bool {
    last_non_additive_tool_change(messages).is_some()
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TranscriptToolSupport {
    None,
    Additions,
    Changes,
}
#[derive(Debug, Default)]
pub struct TranscriptTools<'m> {
    pub request_tools: Vec<&'m Tool>,
    pub folded_system_messages: usize,
}
pub fn resolve_transcript_tools(
    messages: &[Message],
    support: TranscriptToolSupport,
) -> TranscriptTools<'_> {
    let folded = match support {
        TranscriptToolSupport::Changes => return TranscriptTools::default(),
        TranscriptToolSupport::None => systems(messages).count(),
        TranscriptToolSupport::Additions => {
            usize::from(get_initial_system_message(messages).is_some())
                .max(last_non_additive_tool_change(messages).map_or(0, |i| i + 1))
        }
    };
    let mut tools = IndexMap::new();
    for message in systems(messages).take(folded) {
        apply_tools(&mut tools, message);
    }
    TranscriptTools {
        request_tools: tools.into_values().collect(),
        folded_system_messages: folded,
    }
}
