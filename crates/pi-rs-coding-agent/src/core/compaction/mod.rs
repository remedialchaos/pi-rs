//! Pure, offline context compaction and branch summarization.
//!
//! This module ports the behavior of upstream
//! `packages/coding-agent/src/core/compaction/` without any network or model
//! dependency at its core. Messages are exchanged as [`serde_json::Value`]
//! payloads using the same field names as upstream TypeScript messages, so a
//! caller can feed either a parsed `~/.pi/agent/` JSONL session entry or a
//! serialized `pi_rs_ai::Message` / `pi_rs_agent::AgentMessage`.
//!
//! Summarization is always injected as an async closure returning
//! `Result<String, String>`; no provider, API key, or HTTP client is used here.

pub mod branch_summary;
pub mod compact;
pub mod cut_points;
pub mod file_ops;
pub mod serialize;
pub mod token_estimate;

pub use branch_summary::{
    BranchPreparation, BranchSummaryOptions, BranchSummaryResult, generate_branch_summary,
    generate_branch_summary_with_options, prepare_branch_entries,
};
pub use compact::{
    CompactionContext, CompactionOptions, CompactionResult, compact, plan_compaction,
};
pub use cut_points::{CutPoint, find_cut_point, find_cut_point_in_range};
pub use file_ops::{FileLists, FileOpsTracker, format_file_operations};
pub use serialize::{
    BRANCH_SUMMARY_PREAMBLE, BRANCH_SUMMARY_PROMPT, CompactionDetails, SUMMARIZATION_PROMPT,
    SUMMARIZATION_SYSTEM_PROMPT, TOOL_RESULT_MAX_CHARS, TURN_PREFIX_SUMMARIZATION_PROMPT,
    UPDATE_SUMMARIZATION_INSTRUCTIONS, UPDATE_SUMMARIZATION_PROMPT, build_summarization_prompt,
    build_turn_prefix_prompt, compaction_entry_json, details_from_json, details_to_json,
    serialize_conversation, truncate_for_summary,
};
pub use token_estimate::{
    CompactionSettings, ContextUsageEstimate, DEFAULT_COMPACTION_SETTINGS,
    DEFAULT_KEEP_RECENT_TOKENS, DEFAULT_RESERVE_TOKENS, calculate_context_tokens,
    estimate_context_tokens, estimate_message_tokens, estimate_tokens, find_last_assistant_usage,
    should_compact_with,
};

use serde_json::Value;

/// A message in upstream JSON shape. The `role` discriminator and content
/// blocks are identical to those in `pi-rs-ai` / `pi-rs-agent` messages.
pub type MessageValue = Value;

/// Upstream charges a fixed number of characters for an image block.
pub(crate) const ESTIMATED_IMAGE_CHARS: usize = 4800;

/// JavaScript `String.length` in UTF-16 code units. Upstream's `chars / 4`
/// heuristic counts these, not Rust `char`s or UTF-8 bytes.
pub(crate) fn js_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// Returns at most `max_units` UTF-16 code units without splitting a surrogate
/// pair, matching upstream `sliceHead`.
pub(crate) fn slice_head_utf16(text: &str, max_units: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let width = ch.len_utf16();
        if used + width > max_units {
            break;
        }
        out.push(ch);
        used += width;
    }
    out
}

/// Role string from an upstream message payload; `""` when absent.
pub(crate) fn role_of(message: &MessageValue) -> &str {
    message.get("role").and_then(Value::as_str).unwrap_or("")
}

/// String field from an upstream message payload; `""` when absent.
pub(crate) fn field_str<'a>(message: &'a MessageValue, key: &str) -> &'a str {
    message.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Character count of a user/system-like `content` field, which is either a
/// string or an array of `text` / `image` blocks.
pub(crate) fn content_chars(content: &MessageValue) -> usize {
    match content {
        Value::String(text) => js_len(text),
        Value::Array(blocks) => blocks
            .iter()
            .map(|block| match block.get("type").and_then(Value::as_str) {
                Some("text") => block
                    .get("text")
                    .and_then(Value::as_str)
                    .map(js_len)
                    .unwrap_or(0),
                Some("image") => ESTIMATED_IMAGE_CHARS,
                _ => 0,
            })
            .sum(),
        _ => 0,
    }
}

/// Length of compact JSON serialization, matching upstream `JSON.stringify`.
pub(crate) fn compact_json_len(value: &MessageValue) -> usize {
    serde_json::to_string(value)
        .map(|text| js_len(&text))
        .unwrap_or(0)
}
