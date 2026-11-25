//! Serialization helpers and upstream prompt constants.
//!
//! Field names and truncation behavior match upstream `utils.ts` and the
//! compaction/branch-summarization prompt builders.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{MessageValue, js_len, role_of, slice_head_utf16};

/// Details stored on a compaction or branch summary entry. Serialized with
/// upstream camelCase field names.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionDetails {
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// Maximum characters of a tool result kept in a summarization request.
pub const TOOL_RESULT_MAX_CHARS: usize = 2000;

pub const SUMMARIZATION_SYSTEM_PROMPT: &str = r#"You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.

Do NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary."#;

pub const SUMMARIZATION_PROMPT: &str = r#"The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned by user]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, examples, or references needed to continue]
- [Or "(none)" if not applicable]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

pub const UPDATE_SUMMARIZATION_INSTRUCTIONS: &str = r#"Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from "In Progress" to "Done" when completed
- UPDATE "Next Steps" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it

Use this EXACT format:

## Goal
[Preserve existing goals, add new ones if the task expanded]

## Constraints & Preferences
- [Preserve existing, add new ones discovered]

## Progress
### Done
- [x] [Include previously done items AND newly completed items]

### In Progress
- [ ] [Current work - update based on progress]

### Blocked
- [Current blockers - remove if resolved]

## Key Decisions
- **[Decision]**: [Brief rationale] (preserve all previous, add new)

## Next Steps
1. [Update based on current state]

## Critical Context
- [Preserve important context, add new if needed]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

pub const UPDATE_SUMMARIZATION_PROMPT: &str = r#"The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.

Update the existing structured summary with new information. RULES:
- PRESERVE all existing information from the previous summary
- ADD new progress, decisions, and context from the new messages
- UPDATE the Progress section: move items from "In Progress" to "Done" when completed
- UPDATE "Next Steps" based on what was accomplished
- PRESERVE exact file paths, function names, and error messages
- If something is no longer relevant, you may remove it

Use this EXACT format:

## Goal
[Preserve existing goals, add new ones if the task expanded]

## Constraints & Preferences
- [Preserve existing, add new ones discovered]

## Progress
### Done
- [x] [Include previously done items AND newly completed items]

### In Progress
- [ ] [Current work - update based on progress]

### Blocked
- [Current blockers - remove if resolved]

## Key Decisions
- **[Decision]**: [Brief rationale] (preserve all previous, add new)

## Next Steps
1. [Update based on current state]

## Critical Context
- [Preserve important context, add new if needed]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

pub const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = r#"The messages above are earlier context from an ongoing conversation. Later messages are stored separately and do not need to be reconstructed.

Create a concise checkpoint of the user's request and the progress shown above. This checkpoint will be placed before the later messages so the conversation can continue with the necessary context.

## Original Request
[What did the user ask for?]

## Progress So Far
- [Key decisions and work completed in these messages]

## Context Needed to Continue
- [Information from these messages needed to understand the later work]

Only summarize information explicitly present above. Do not infer or recreate later messages."#;

pub const BRANCH_SUMMARY_PREAMBLE: &str = "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\n";

pub const BRANCH_SUMMARY_PROMPT: &str = r#"Create a structured summary of this conversation branch for context when returning later.

Use this EXACT format:

## Goal
[What was the user trying to accomplish in this branch?]

## Constraints & Preferences
- [Any constraints, preferences, or requirements mentioned]
- [Or "(none)" if none were mentioned]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Work that was started but not finished]

### Blocked
- [Issues preventing progress, if any]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [What should happen next to continue this work]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

/// Upstream `truncateForSummary`: keep a prefix and report the omitted count
/// in UTF-16 code units.
pub fn truncate_for_summary(text: &str, max_chars: usize) -> String {
    let length = js_len(text);
    if length <= max_chars {
        return text.to_string();
    }
    format!(
        "{}\n\n[... {} more characters truncated]",
        slice_head_utf16(text, max_chars),
        length - max_chars
    )
}

/// Upstream `contentText(content, separator)`.
fn content_text(content: &Value, separator: &str) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(separator),
        _ => String::new(),
    }
}

/// Upstream `serializeConversation`: text form fed to the summarizer.
pub fn serialize_conversation(messages: &[MessageValue]) -> String {
    let mut parts = Vec::new();
    for message in messages {
        match role_of(message) {
            "user" => {
                let content = content_text(message.get("content").unwrap_or(&Value::Null), "");
                if !content.is_empty() {
                    parts.push(format!("[User]: {content}"));
                }
            }
            "assistant" => {
                let mut tool_calls = Vec::new();
                if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                    for block in blocks {
                        if block.get("type").and_then(Value::as_str) != Some("toolCall") {
                            continue;
                        }
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                        let arguments = match block.get("arguments") {
                            Some(Value::Object(map)) => map
                                .iter()
                                .map(|(key, value)| {
                                    serde_json::to_string(value)
                                        .map(|encoded| format!("{key}={encoded}"))
                                        .unwrap_or_else(|_| format!("{key}=null"))
                                })
                                .collect::<Vec<_>>()
                                .join(", "),
                            _ => String::new(),
                        };
                        tool_calls.push(format!("{name}({arguments})"));
                    }
                    if blocks
                        .iter()
                        .any(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                    {
                        parts.push(format!(
                            "[Assistant]: {}",
                            content_text(message.get("content").unwrap_or(&Value::Null), "\n")
                        ));
                    }
                }
                if !tool_calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", tool_calls.join("; ")));
                }
            }
            "toolResult" => {
                let content = content_text(message.get("content").unwrap_or(&Value::Null), "");
                if !content.is_empty() {
                    parts.push(format!(
                        "[Tool result]: {}",
                        truncate_for_summary(&content, TOOL_RESULT_MAX_CHARS)
                    ));
                }
            }
            _ => {}
        }
    }
    parts.join("\n\n")
}

/// Prompt for a history or update summary, using upstream prompt text.
pub fn build_summarization_prompt(
    messages: &[MessageValue],
    previous_summary: Option<&str>,
    instructions: Option<&str>,
) -> String {
    let conversation = serialize_conversation(messages);
    let mut prompt = format!("<conversation>\n{conversation}\n</conversation>\n\n");
    if let Some(previous) = previous_summary {
        prompt.push_str(&format!(
            "<previous-summary>\n{previous}\n</previous-summary>\n\n"
        ));
    }
    prompt.push_str(if previous_summary.is_some() {
        UPDATE_SUMMARIZATION_PROMPT
    } else {
        SUMMARIZATION_PROMPT
    });
    if let Some(extra) = instructions {
        prompt.push_str(&format!("\n\nAdditional focus: {extra}"));
    }
    prompt
}

/// Prompt for a turn-prefix summary when a cut splits a turn.
pub fn build_turn_prefix_prompt(messages: &[MessageValue], instructions: Option<&str>) -> String {
    let conversation = serialize_conversation(messages);
    let mut prompt = format!(
        "# Conversation\n{conversation}\n\n# Instructions\n{TURN_PREFIX_SUMMARIZATION_PROMPT}"
    );
    if let Some(extra) = instructions {
        prompt.push_str(&format!("\n\nAdditional focus: {extra}"));
    }
    prompt
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

pub fn details_to_json(details: &CompactionDetails) -> Value {
    json!({
        "readFiles": details.read_files,
        "modifiedFiles": details.modified_files,
    })
}

pub fn details_from_json(value: &Value) -> CompactionDetails {
    CompactionDetails {
        read_files: string_array(value.get("readFiles")),
        modified_files: string_array(value.get("modifiedFiles")),
    }
}

/// A JSONL `compaction` entry with upstream field names.
pub fn compaction_entry_json(
    id: &str,
    parent_id: Option<&str>,
    timestamp: &str,
    summary: &str,
    first_kept_entry_id: &str,
    tokens_before: usize,
    details: Option<&CompactionDetails>,
) -> Value {
    let mut entry = json!({
        "type": "compaction",
        "id": id,
        "timestamp": timestamp,
        "summary": summary,
        "firstKeptEntryId": first_kept_entry_id,
        "tokensBefore": tokens_before,
    });
    if let Some(map) = entry.as_object_mut() {
        if let Some(parent) = parent_id {
            let _previous = map.insert("parentId".to_string(), json!(parent));
        }
        if let Some(details) = details {
            let _previous = map.insert("details".to_string(), details_to_json(details));
        }
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_long_tool_results_with_upstream_marker() {
        let messages = vec![json!({
            "role": "toolResult",
            "toolCallId": "tc1",
            "toolName": "read",
            "content": [{"type": "text", "text": "x".repeat(5000)}]
        })];
        let result = serialize_conversation(&messages);
        assert!(result.starts_with("[Tool result]: "));
        assert!(result.contains("\n\n[... 3000 more characters truncated]"));
        assert!(!result.contains(&"x".repeat(3000)));
        assert!(result.contains(&"x".repeat(2000)));
    }

    #[test]
    fn does_not_truncate_short_tool_results_or_regular_messages() {
        let messages = vec![
            json!({
                "role": "toolResult",
                "toolCallId": "tc1",
                "toolName": "read",
                "content": [{"type": "text", "text": "x".repeat(1500)}]
            }),
            json!({"role": "user", "content": [{"type": "text", "text": "y".repeat(5000)}]}),
        ];
        let result = serialize_conversation(&messages);
        assert_eq!(
            result,
            format!(
                "[Tool result]: {}\n\n[User]: {}",
                "x".repeat(1500),
                "y".repeat(5000)
            )
        );
        assert!(!result.contains("truncated"));
    }

    #[test]
    fn omit_thinking_and_format_tool_calls() {
        let messages = vec![json!({
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "private reasoning", "thinkingSignature": "sig"},
                {"type": "text", "text": "visible reply"},
                {"type": "toolCall", "id": "tc1", "name": "read", "arguments": {"path": "a.ts"}}
            ]
        })];
        assert_eq!(
            serialize_conversation(&messages),
            "[Assistant]: visible reply\n\n[Assistant tool calls]: read(path=\"a.ts\")"
        );
    }

    #[test]
    fn details_use_upstream_camel_case_field_names() {
        let details = CompactionDetails {
            read_files: vec!["a.rs".to_string()],
            modified_files: vec!["b.rs".to_string()],
        };
        let json = details_to_json(&details);
        assert_eq!(json["readFiles"][0], "a.rs");
        assert_eq!(json["modifiedFiles"][0], "b.rs");
        assert_eq!(details_from_json(&json), details);
        assert_eq!(
            serde_json::to_string(&details).unwrap_or_default(),
            "{\"readFiles\":[\"a.rs\"],\"modifiedFiles\":[\"b.rs\"]}"
        );
    }

    #[test]
    fn compaction_entry_has_upstream_field_names() {
        let details = CompactionDetails {
            read_files: vec![],
            modified_files: vec!["b.rs".to_string()],
        };
        let entry = compaction_entry_json(
            "entry-1",
            Some("parent-1"),
            "2026-01-01T00:00:00.000Z",
            "summary text",
            "entry-0",
            1234,
            Some(&details),
        );
        assert_eq!(entry["type"], "compaction");
        assert_eq!(entry["firstKeptEntryId"], "entry-0");
        assert_eq!(entry["tokensBefore"], 1234);
        assert_eq!(entry["details"]["modifiedFiles"][0], "b.rs");
    }

    #[test]
    fn prompts_include_previous_summary_and_instructions() {
        let messages = vec![json!({"role": "user", "content": "hi"})];
        let prompt =
            build_summarization_prompt(&messages, Some("old summary"), Some("focus on tests"));
        assert!(prompt.contains("<conversation>\n[User]: hi\n</conversation>"));
        assert!(prompt.contains("<previous-summary>\nold summary\n</previous-summary>"));
        assert!(prompt.contains(UPDATE_SUMMARIZATION_INSTRUCTIONS));
        assert!(prompt.contains("Additional focus: focus on tests"));
        let turn = build_turn_prefix_prompt(&messages, None);
        assert!(turn.contains("# Conversation\n[User]: hi"));
        assert!(turn.contains(TURN_PREFIX_SUMMARIZATION_PROMPT));
    }
}
