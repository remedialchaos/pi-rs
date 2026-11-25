//! Main compaction entry point with an injected summarizer.
//!
//! No provider call is made here: `compact` decides which messages to discard,
//! passes them to `summarize_fn`, and assembles the result. The session manager
//! is responsible for persisting the returned summary and retained messages.

use std::future::Future;

use pi_rs_ai::{Model, ThinkingLevel};
use serde_json::Value;

use super::{
    CompactionDetails, CutPoint, FileOpsTracker, MessageValue, file_ops::format_file_operations,
    find_cut_point,
};

#[derive(Clone, Debug, Default)]
pub struct CompactionContext {
    /// Context-visible messages in chronological order.
    pub messages: Vec<MessageValue>,
    /// File lists carried over from the previous compaction, if any.
    pub previous_details: Option<CompactionDetails>,
}

impl CompactionContext {
    pub fn new(messages: Vec<MessageValue>) -> Self {
        Self {
            messages,
            previous_details: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CompactionOptions {
    pub keep_recent_tokens: usize,
    pub instructions: Option<String>,
    pub model: Option<Model>,
    pub previous_summary: Option<String>,
    pub thinking_level: Option<ThinkingLevel>,
    pub reserve_tokens: usize,
    pub context_window: Option<usize>,
}

impl Default for CompactionOptions {
    fn default() -> Self {
        Self {
            keep_recent_tokens: super::DEFAULT_KEEP_RECENT_TOKENS,
            instructions: None,
            model: None,
            previous_summary: None,
            thinking_level: None,
            reserve_tokens: super::DEFAULT_RESERVE_TOKENS,
            context_window: None,
        }
    }
}

impl CompactionOptions {
    /// Options seeded from a model's context window, with no API call implied.
    pub fn for_model(model: Model) -> Self {
        let context_window = usize::try_from(model.context_window).ok();
        Self {
            model: Some(model),
            context_window,
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionResult {
    /// Generated summary including any appended file-operation sections.
    pub summary: String,
    /// Messages retained after the cut, in chronological order.
    pub messages_kept: Vec<MessageValue>,
    /// Id of the first retained message (upstream `firstKeptEntryId`).
    pub first_kept_entry_id: String,
    /// File lists recorded while summarizing; persisted on the compaction entry.
    pub details: CompactionDetails,
}

fn message_id(message: &Value, index: usize) -> String {
    message
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("msg-{index}"))
}

/// Generate a compaction result using the injected summarizer.
///
/// `summarize_fn` receives only the messages being discarded. For a split turn
/// it is called twice: once for the history and once for the turn prefix. Its
/// error is returned verbatim to the caller.
pub async fn compact<F, Fut>(
    context: &CompactionContext,
    options: &CompactionOptions,
    summarize_fn: F,
) -> Result<CompactionResult, String>
where
    F: Fn(Vec<MessageValue>) -> Fut,
    Fut: Future<Output = Result<String, String>>,
{
    if context.messages.is_empty() {
        return Err("No messages to compact".to_string());
    }

    let cut = find_cut_point(&context.messages, options.keep_recent_tokens);
    let turn_start = cut.turn_start_index.unwrap_or(cut.index);
    let history_end = if cut.is_split_turn {
        turn_start
    } else {
        cut.index
    };
    let history = context.messages[..history_end].to_vec();
    let turn_prefix = if cut.is_split_turn {
        context.messages[turn_start..cut.index].to_vec()
    } else {
        Vec::new()
    };

    if history.is_empty() && turn_prefix.is_empty() {
        return Err("No content to summarize".to_string());
    }

    let mut summary = if cut.is_split_turn && !turn_prefix.is_empty() {
        let history_text = if history.is_empty() {
            options
                .previous_summary
                .clone()
                .unwrap_or_else(|| "No prior history.".to_string())
        } else {
            summarize_fn(history.clone()).await?
        };
        let prefix_text = summarize_fn(turn_prefix.clone()).await?;
        format!("{history_text}\n\n---\n\n**Turn Context (split turn):**\n\n{prefix_text}")
    } else {
        summarize_fn(history.clone()).await?
    };

    let mut tracker = FileOpsTracker::new();
    if let Some(previous) = context.previous_details.as_ref() {
        tracker.merge_previous_details(previous);
    }
    for message in history.iter().chain(turn_prefix.iter()) {
        tracker.extract_from_message(message);
    }
    let lists = tracker.compute_file_lists();
    summary.push_str(&format_file_operations(
        &lists.read_files,
        &lists.modified_files,
    ));

    Ok(CompactionResult {
        summary,
        messages_kept: context.messages[cut.index..].to_vec(),
        first_kept_entry_id: message_id(&context.messages[cut.index], cut.index),
        details: CompactionDetails {
            read_files: lists.read_files,
            modified_files: lists.modified_files,
        },
    })
}

/// Kept for callers that only need the cut decision without a summary.
pub fn plan_compaction(context: &CompactionContext, options: &CompactionOptions) -> CutPoint {
    find_cut_point(&context.messages, options.keep_recent_tokens)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;

    fn user(id: &str, text: &str) -> MessageValue {
        json!({"role": "user", "id": id, "content": text})
    }

    fn assistant(id: &str, text: &str) -> MessageValue {
        json!({
            "role": "assistant",
            "id": id,
            "content": [{"type": "text", "text": text}],
            "stopReason": "stop"
        })
    }

    fn constant_summarizer(
        text: &'static str,
    ) -> impl Fn(Vec<MessageValue>) -> std::future::Ready<Result<String, String>> {
        move |_| std::future::ready(Ok(text.to_string()))
    }

    #[tokio::test]
    async fn compact_returns_summary_kept_messages_and_first_id() {
        let context = CompactionContext::new(vec![
            user("u1", "first question"),
            assistant("a1", "first answer"),
            user("u2", "second question"),
            assistant("a2", "second answer"),
        ]);
        let options = CompactionOptions {
            keep_recent_tokens: 8,
            ..CompactionOptions::default()
        };
        let result = compact(&context, &options, constant_summarizer("A summary")).await;
        let result = result.unwrap_or_else(|error| panic!("compact failed: {error}"));
        assert_eq!(result.summary, "A summary");
        assert_eq!(result.messages_kept.len(), 2);
        assert_eq!(result.first_kept_entry_id, "u2");
        assert_eq!(result.details, CompactionDetails::default());
    }

    #[tokio::test]
    async fn compact_merges_history_and_turn_prefix_summaries() {
        let context = CompactionContext::new(vec![
            user("u1", &"old history ".repeat(20)),
            assistant("a1", &"old answer ".repeat(20)),
            user("u2", "current request"),
            assistant("a2", "in-progress answer"),
        ]);
        let options = CompactionOptions {
            keep_recent_tokens: 1,
            ..CompactionOptions::default()
        };
        let seen: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let summarizer = move |messages: Vec<MessageValue>| {
            let recorder = recorder.clone();
            async move {
                let mut calls = recorder.lock().map_err(|_| "poisoned".to_string())?;
                calls.push(messages.len());
                drop(calls);
                Ok(format!("summary-of-{}", messages.len()))
            }
        };
        let result = compact(&context, &options, summarizer).await;
        let result = result.unwrap_or_else(|error| panic!("compact failed: {error}"));
        let calls = match seen.lock() {
            Ok(guard) => guard.clone(),
            Err(_) => Vec::new(),
        };
        assert_eq!(calls, vec![2, 1]);
        assert!(result.summary.contains("summary-of-2"));
        assert!(result.summary.contains("**Turn Context (split turn):**"));
        assert!(result.summary.contains("summary-of-1"));
        assert_eq!(result.messages_kept.len(), 1);
        assert_eq!(result.first_kept_entry_id, "a2");
    }

    #[tokio::test]
    async fn compact_appends_file_operations_from_summarized_messages() {
        let context = CompactionContext::new(vec![
            json!({
                "role": "user",
                "id": "u1",
                "content": "edit the files"
            }),
            json!({
                "role": "assistant",
                "id": "a1",
                "content": [
                    {"type": "toolCall", "id": "t1", "name": "read", "arguments": {"path": "read.rs"}},
                    {"type": "toolCall", "id": "t2", "name": "edit", "arguments": {"path": "edit.rs"}}
                ],
                "stopReason": "toolUse"
            }),
            user("u2", "latest"),
        ]);
        let options = CompactionOptions {
            keep_recent_tokens: 1,
            ..CompactionOptions::default()
        };
        let result = compact(&context, &options, constant_summarizer("Summary")).await;
        let result = result.unwrap_or_else(|error| panic!("compact failed: {error}"));
        assert!(
            result
                .summary
                .contains("<read-files>\nread.rs\n</read-files>")
        );
        assert!(
            result
                .summary
                .contains("<modified-files>\nedit.rs\n</modified-files>")
        );
        assert_eq!(result.details.read_files, vec!["read.rs".to_string()]);
        assert_eq!(result.details.modified_files, vec!["edit.rs".to_string()]);
    }

    #[tokio::test]
    async fn compact_carries_previous_file_details() {
        let context = CompactionContext {
            messages: vec![
                user("u1", "old"),
                assistant("a1", "answer"),
                user("u2", "new"),
            ],
            previous_details: Some(CompactionDetails {
                read_files: vec!["earlier.rs".to_string()],
                modified_files: vec!["changed.rs".to_string()],
            }),
        };
        let options = CompactionOptions {
            keep_recent_tokens: 2,
            ..CompactionOptions::default()
        };
        let result = compact(&context, &options, constant_summarizer("Summary")).await;
        let result = result.unwrap_or_else(|error| panic!("compact failed: {error}"));
        assert_eq!(result.details.read_files, vec!["earlier.rs".to_string()]);
        assert_eq!(
            result.details.modified_files,
            vec!["changed.rs".to_string()]
        );
    }

    #[tokio::test]
    async fn compact_propagates_summarizer_errors() {
        let context = CompactionContext::new(vec![
            user("u1", "question"),
            assistant("a1", "answer"),
            user("u2", "follow up"),
        ]);
        let options = CompactionOptions {
            keep_recent_tokens: 4,
            ..CompactionOptions::default()
        };
        let failing =
            |_messages: Vec<MessageValue>| async { Err("summarizer offline".to_string()) };
        let error = compact(&context, &options, failing)
            .await
            .err()
            .unwrap_or_else(|| "expected an error".to_string());
        assert_eq!(error, "summarizer offline");
    }

    #[tokio::test]
    async fn compact_rejects_empty_context() {
        let context = CompactionContext::default();
        let result = compact(
            &context,
            &CompactionOptions::default(),
            constant_summarizer("x"),
        )
        .await;
        assert!(result.is_err());
        let point = plan_compaction(&context, &CompactionOptions::default());
        assert_eq!(point.index, 0);
    }
}
