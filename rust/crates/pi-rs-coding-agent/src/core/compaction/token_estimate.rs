//! Token estimates and compaction settings.
//!
//! Upstream uses a conservative `chars / 4` heuristic. Character counts are
//! JavaScript UTF-16 code units, matching upstream `String.length`.

use serde_json::Value;

use super::{MessageValue, compact_json_len, content_chars, field_str, js_len, role_of};

/// Upstream default `compaction.keepRecentTokens` from `settings.json`.
pub const DEFAULT_KEEP_RECENT_TOKENS: usize = 20_000;
/// Upstream default `compaction.reserveTokens` from `settings.json`.
pub const DEFAULT_RESERVE_TOKENS: usize = 16_384;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompactionSettings {
    pub enabled: bool,
    pub reserve_tokens: usize,
    pub keep_recent_tokens: usize,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        DEFAULT_COMPACTION_SETTINGS
    }
}

pub const DEFAULT_COMPACTION_SETTINGS: CompactionSettings = CompactionSettings {
    enabled: true,
    reserve_tokens: DEFAULT_RESERVE_TOKENS,
    keep_recent_tokens: DEFAULT_KEEP_RECENT_TOKENS,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ContextUsageEstimate {
    pub tokens: usize,
    pub usage_tokens: usize,
    pub trailing_tokens: usize,
    pub last_usage_index: Option<usize>,
}

/// Plain-text `chars / 4` estimate, rounded up like upstream `Math.ceil`.
pub fn estimate_tokens(text: &str) -> usize {
    js_len(text).div_ceil(4)
}

/// Upstream `estimateTokens(message)` for every role handled by the agent.
pub fn estimate_message_tokens(message: &MessageValue) -> usize {
    let chars = match role_of(message) {
        "system" => {
            let mut chars = message.get("content").map(content_chars).unwrap_or(0);
            if let Some(sections) = message.get("sections").and_then(Value::as_object) {
                for section in sections.values() {
                    if let Some(text) = section.as_str() {
                        chars += js_len(text);
                    }
                }
            }
            if let Some(tools_added) = message.get("toolsAdded") {
                chars += compact_json_len(tools_added);
            }
            chars
        }
        "user" => message.get("content").map(content_chars).unwrap_or(0),
        "assistant" => {
            let mut chars = 0;
            if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            chars += js_len(field_str(block, "text"));
                        }
                        Some("thinking") => {
                            chars += js_len(field_str(block, "thinking"));
                        }
                        Some("toolCall") => {
                            chars += js_len(field_str(block, "name"));
                            if let Some(arguments) = block.get("arguments") {
                                chars += compact_json_len(arguments);
                            }
                        }
                        _ => {}
                    }
                }
            }
            chars
        }
        "custom" | "toolResult" => message.get("content").map(content_chars).unwrap_or(0),
        "bashExecution" => {
            js_len(field_str(message, "command")) + js_len(field_str(message, "output"))
        }
        "branchSummary" | "compactionSummary" => js_len(field_str(message, "summary")),
        _ => 0,
    };
    chars.div_ceil(4)
}

/// Upstream `calculateContextTokens`: trust `totalTokens`, fall back to the
/// sum of the four usage components when it is zero or absent.
pub fn calculate_context_tokens(usage: &MessageValue) -> usize {
    let component = |key: &str| usage.get(key).and_then(Value::as_f64).unwrap_or(0.0);
    let total = usage
        .get("totalTokens")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if total > 0.0 {
        total as usize
    } else {
        (component("input")
            + component("output")
            + component("cacheRead")
            + component("cacheWrite")) as usize
    }
}

/// Returns an assistant message usable as a context anchor: not aborted or
/// errored and carrying non-zero usage.
pub fn find_last_assistant_usage(messages: &[MessageValue]) -> Option<&MessageValue> {
    for message in messages.iter().rev() {
        if role_of(message) != "assistant" {
            continue;
        }
        let stop_reason = field_str(message, "stopReason");
        if stop_reason == "aborted" || stop_reason == "error" {
            continue;
        }
        let Some(usage) = message.get("usage") else {
            continue;
        };
        if calculate_context_tokens(usage) > 0 {
            return Some(usage);
        }
    }
    None
}

fn last_usage_index(messages: &[MessageValue]) -> Option<usize> {
    (0..messages.len()).rev().find(|&index| {
        let message = &messages[index];
        role_of(message) == "assistant"
            && !matches!(field_str(message, "stopReason"), "aborted" | "error")
            && message
                .get("usage")
                .is_some_and(|usage| calculate_context_tokens(usage) > 0)
    })
}

/// Upstream `estimateContextTokens`: anchor on the last valid assistant usage,
/// then estimate only the trailing messages.
pub fn estimate_context_tokens(messages: &[MessageValue]) -> ContextUsageEstimate {
    let Some(index) = last_usage_index(messages) else {
        let estimated: usize = messages.iter().map(estimate_message_tokens).sum();
        return ContextUsageEstimate {
            tokens: estimated,
            usage_tokens: 0,
            trailing_tokens: estimated,
            last_usage_index: None,
        };
    };

    let usage_tokens =
        calculate_context_tokens(messages[index].get("usage").unwrap_or(&Value::Null));
    let trailing_tokens = messages[index + 1..]
        .iter()
        .map(estimate_message_tokens)
        .sum();
    ContextUsageEstimate {
        tokens: usage_tokens + trailing_tokens,
        usage_tokens,
        trailing_tokens,
        last_usage_index: Some(index),
    }
}

/// Upstream `shouldCompact` without a settings type.
pub fn should_compact_with(
    context_tokens: usize,
    context_window: usize,
    settings: CompactionSettings,
) -> bool {
    settings.enabled && context_tokens > context_window.saturating_sub(settings.reserve_tokens)
}

impl CompactionSettings {
    pub fn should_compact(&self, context_tokens: usize, context_window: usize) -> bool {
        should_compact_with(context_tokens, context_window, *self)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn estimate_tokens_rounds_chars_up() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        assert_eq!(estimate_tokens("a".repeat(4000).as_str()), 1000);
    }

    #[test]
    fn estimate_tokens_uses_utf16_code_units() {
        assert_eq!(estimate_tokens("😀😀"), 1);
        assert_eq!(estimate_tokens("😀😀😀😀😀"), 3);
    }

    #[test]
    fn estimate_message_tokens_user_string_and_blocks() {
        let string_user = json!({"role": "user", "content": "12345678"});
        assert_eq!(estimate_message_tokens(&string_user), 2);
        let blocks = json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "1234"},
                {"type": "image", "data": "ignored"}
            ]
        });
        assert_eq!(estimate_message_tokens(&blocks), 1201);
    }

    #[test]
    fn estimate_message_tokens_assistant_blocks() {
        let assistant = json!({
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "1234"},
                {"type": "text", "text": "12345678"},
                {"type": "toolCall", "name": "read", "arguments": {"path": "a.ts"}}
            ]
        });
        let expected = js_len("1234") + js_len("12345678") + js_len("read") + 15;
        assert_eq!(estimate_message_tokens(&assistant), expected.div_ceil(4));
    }

    #[test]
    fn estimate_message_tokens_summary_and_bash_roles() {
        let summary = json!({"role": "branchSummary", "summary": "0123456789"});
        assert_eq!(estimate_message_tokens(&summary), 3);
        let bash = json!({"role": "bashExecution", "command": "1234", "output": "12345678"});
        assert_eq!(estimate_message_tokens(&bash), 3);
    }

    #[test]
    fn calculate_context_tokens_prefers_total_then_components() {
        let with_total =
            json!({"input": 10, "output": 10, "cacheRead": 10, "cacheWrite": 10, "totalTokens": 7});
        assert_eq!(calculate_context_tokens(&with_total), 7);
        let without_total = json!({"input": 10, "output": 20, "cacheRead": 30, "cacheWrite": 40});
        assert_eq!(calculate_context_tokens(&without_total), 100);
    }

    #[test]
    fn estimate_context_tokens_anchors_on_last_valid_usage() {
        let messages = vec![
            json!({"role": "user", "content": "1234"}),
            json!({
                "role": "assistant",
                "content": [{"type": "text", "text": "ok"}],
                "stopReason": "stop",
                "usage": {"totalTokens": 150}
            }),
            json!({"role": "user", "content": "12345678"}),
            json!({
                "role": "assistant",
                "content": [{"type": "text", "text": "partial"}],
                "stopReason": "stop",
                "usage": {"totalTokens": 0, "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}
            }),
        ];
        let estimate = estimate_context_tokens(&messages);
        assert_eq!(estimate.last_usage_index, Some(1));
        assert_eq!(estimate.usage_tokens, 150);
        assert_eq!(estimate.trailing_tokens, 4);
        assert_eq!(estimate.tokens, 154);
        assert!(find_last_assistant_usage(&messages).is_some());
    }

    #[test]
    fn find_last_assistant_usage_skips_messages_without_usage() {
        let messages = vec![
            json!({
                "role": "assistant",
                "content": [{"type": "text", "text": "old answer"}],
                "usage": {"totalTokens": 42},
                "stopReason": "stop"
            }),
            json!({
                "role": "assistant",
                "content": [{"type": "text", "text": "no usage"}],
                "stopReason": "stop"
            }),
        ];
        let usage = find_last_assistant_usage(&messages);
        assert_eq!(
            usage.and_then(|value| value.get("totalTokens")),
            Some(&json!(42))
        );
    }

    #[test]
    fn should_compact_respects_enabled_and_reserve() {
        let settings = CompactionSettings {
            enabled: true,
            reserve_tokens: 10_000,
            keep_recent_tokens: 20_000,
        };
        assert!(settings.should_compact(95_000, 100_000));
        assert!(!settings.should_compact(89_000, 100_000));
        assert!(
            !CompactionSettings {
                enabled: false,
                ..settings
            }
            .should_compact(95_000, 100_000)
        );
    }
}
