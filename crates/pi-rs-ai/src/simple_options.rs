use serde::Serialize;

use crate::model::{Model, SamplingParams, ThinkingLevel};
use crate::request::StreamOptions;
use crate::transcript::system_message_text;
use crate::types::{
    AssistantContent, InputContent, Message, StringOrBlocks, TranscriptContext, Usage,
};

const CONTEXT_SAFETY_TOKENS: u64 = 4096;
const MIN_OUTPUT_WITH_RATIO_MARGIN: u64 = 32768;
pub const MIN_ANSWER_TOKENS: u64 = 1024;

fn chars(text: &str) -> u64 {
    (if text.is_ascii() {
        text.len()
    } else {
        text.encode_utf16().count()
    }) as u64
}
fn from_chars(length: u64) -> u64 {
    length.saturating_mul(2).div_ceil(7)
}
fn json_chars(value: &impl Serialize) -> u64 {
    serde_json::to_string(value).map_or(16, |s| chars(&s))
}
pub fn estimate_text_tokens(text: &str) -> u64 {
    from_chars(chars(text))
}
fn content_chars(blocks: &[InputContent]) -> u64 {
    blocks
        .iter()
        .map(|b| match b {
            InputContent::Text(t) => chars(&t.text),
            InputContent::Image(_) => 4800,
        })
        .sum()
}
pub fn calculate_context_tokens(usage: &Usage) -> f64 {
    if usage.total_tokens != 0.0 {
        usage.total_tokens
    } else {
        usage.input + usage.output + usage.cache_read + usage.cache_write
    }
}
pub fn estimate_message_tokens(message: &Message) -> u64 {
    match message {
        Message::System(system) => {
            estimate_text_tokens(&system_message_text(system))
                + system
                    .tools_added
                    .as_ref()
                    .filter(|v| !v.is_empty())
                    .map_or(0, |v| from_chars(json_chars(v)))
                + system
                    .tools_removed
                    .as_ref()
                    .filter(|v| !v.is_empty())
                    .map_or(0, |v| from_chars(json_chars(v)))
        }
        Message::User(user) => match &user.content {
            StringOrBlocks::String(text) => estimate_text_tokens(text),
            StringOrBlocks::Blocks(blocks) => from_chars(content_chars(blocks)),
        },
        Message::ToolResult(tool) => from_chars(content_chars(&tool.content)),
        Message::Assistant(a) => from_chars(
            a.content
                .iter()
                .map(|b| match b {
                    AssistantContent::Text(t) => chars(&t.text),
                    AssistantContent::Thinking(t) => chars(&t.thinking),
                    AssistantContent::ToolCall(t) => chars(&t.name) + json_chars(&t.arguments),
                })
                .sum(),
        ),
    }
}
#[derive(Debug, Default, PartialEq)]
pub struct ContextUsageEstimate {
    pub tokens: f64,
    pub usage_tokens: f64,
    pub trailing_tokens: u64,
    pub last_usage_index: Option<usize>,
}
pub fn estimate_context_tokens(messages: &[Message]) -> ContextUsageEstimate {
    let mut latest = 0;
    let mut usage_tokens = 0.0;
    let mut last_usage_index = None;
    for (i, message) in messages.iter().enumerate() {
        let timestamp = match message {
            Message::System(m) => m.timestamp,
            Message::User(m) => m.timestamp,
            Message::ToolResult(m) => m.timestamp,
            Message::Assistant(a) => {
                if a.timestamp >= latest && !a.stop_reason.failed() {
                    let total = calculate_context_tokens(&a.usage);
                    if total > 0.0 {
                        usage_tokens = total;
                        last_usage_index = Some(i);
                    }
                }
                a.timestamp
            }
        };
        latest = latest.max(timestamp);
    }
    let trailing_tokens = messages[last_usage_index.map_or(0, |i| i + 1)..]
        .iter()
        .map(estimate_message_tokens)
        .sum();
    ContextUsageEstimate {
        tokens: usage_tokens + trailing_tokens as f64,
        usage_tokens,
        trailing_tokens,
        last_usage_index,
    }
}

pub fn clamp_max_tokens_to_context(
    model: &Model,
    context: &TranscriptContext,
    max_tokens: u64,
) -> u64 {
    if model.context_window == 0 {
        return max_tokens.max(1);
    }
    let input = estimate_context_tokens(context.messages()).tokens;
    let margin = (CONTEXT_SAFETY_TOKENS as f64).max((input * 0.15).ceil());
    let mut room = model.context_window as f64 - input - margin;
    if room < MIN_OUTPUT_WITH_RATIO_MARGIN as f64 {
        room = (model.context_window as f64 - input - CONTEXT_SAFETY_TOKENS as f64)
            .min(MIN_OUTPUT_WITH_RATIO_MARGIN as f64);
    }
    max_tokens.min(room.max(1.0) as u64)
}
pub fn resolve_sampling_params(
    model: &Model,
    level: ThinkingLevel,
    supplied: SamplingParams,
) -> SamplingParams {
    let mut params = model.sampling_params.clone().unwrap_or_default();
    if let Some(level_params) = model
        .sampling_params_by_thinking_level
        .as_ref()
        .and_then(|map| map.get(&model.clamp_thinking_level(level)))
    {
        params.extend(level_params.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    params.extend(supplied);
    params
}
pub fn build_base_options(
    model: &Model,
    context: &TranscriptContext,
    mut options: StreamOptions,
) -> StreamOptions {
    options.max_tokens = Some(clamp_max_tokens_to_context(
        model,
        context,
        options.max_tokens.unwrap_or(model.max_tokens),
    ));
    options.sampling_params = resolve_sampling_params(
        model,
        options.reasoning.unwrap_or(ThinkingLevel::Off),
        options.sampling_params,
    );
    options
}
pub fn thinking_budget_for_level(
    level: ThinkingLevel,
    custom: &indexmap::IndexMap<ThinkingLevel, u64>,
) -> u64 {
    let level = match level {
        ThinkingLevel::Xhigh | ThinkingLevel::Max => ThinkingLevel::High,
        other => other,
    };
    custom.get(&level).copied().unwrap_or(match level {
        ThinkingLevel::Off => 0,
        ThinkingLevel::Minimal => 1024,
        ThinkingLevel::Low => 2048,
        ThinkingLevel::Medium => 8192,
        _ => 16384,
    })
}
pub fn clamp_thinking_budget_to_answer_room(budget: u64, ceiling: u64) -> u64 {
    budget.min(ceiling.saturating_sub(MIN_ANSWER_TOKENS))
}
pub fn adjust_max_tokens_for_thinking(
    cap: Option<u64>,
    model_max: u64,
    level: ThinkingLevel,
    custom: &indexmap::IndexMap<ThinkingLevel, u64>,
) -> (u64, u64) {
    let mut budget = thinking_budget_for_level(level, custom);
    let limit = cap.map_or(model_max, |cap| cap.saturating_add(budget).min(model_max));
    if limit <= budget {
        budget = clamp_thinking_budget_to_answer_room(budget, limit);
    }
    (limit, budget)
}
