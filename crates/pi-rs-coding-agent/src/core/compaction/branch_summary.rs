//! Branch summarization with an injected summarizer.
//!
//! Ports the pure parts of upstream `branch-summarization.ts`: entry/message
//! preparation under a token budget, cumulative file tracking, prompt preamble,
//! and result shaping. Provider calls and session-tree traversal stay in the
//! session layer.

use std::future::Future;

use pi_rs_ai::{Model, ThinkingLevel};
use serde_json::Value;

use super::{
    BRANCH_SUMMARY_PREAMBLE, BRANCH_SUMMARY_PROMPT, FileOpsTracker, MessageValue,
    estimate_message_tokens, file_ops::format_file_operations, role_of,
};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BranchSummaryResult {
    pub summary: Option<String>,
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
    pub aborted: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct BranchPreparation {
    pub messages: Vec<MessageValue>,
    pub file_ops: FileOpsTracker,
    pub total_tokens: usize,
}

#[derive(Clone, Debug)]
pub struct BranchSummaryOptions {
    pub custom_instructions: Option<String>,
    /// When true, custom instructions replace the default prompt instead of
    /// being appended to it.
    pub replace_instructions: bool,
    pub context_window: usize,
    pub reserve_tokens: usize,
    pub model: Option<Model>,
    pub thinking_level: Option<ThinkingLevel>,
}

impl Default for BranchSummaryOptions {
    fn default() -> Self {
        Self {
            custom_instructions: None,
            replace_instructions: false,
            context_window: 128_000,
            reserve_tokens: 16_384,
            model: None,
            thinking_level: None,
        }
    }
}

impl BranchSummaryOptions {
    pub fn for_model(model: Model) -> Self {
        let context_window = usize::try_from(model.context_window)
            .ok()
            .unwrap_or(128_000);
        Self {
            model: Some(model),
            context_window,
            ..Self::default()
        }
    }

    /// Upstream prompt text actually used for the summarizer request.
    pub fn instructions(&self) -> String {
        match (&self.custom_instructions, self.replace_instructions) {
            (Some(custom), true) => custom.clone(),
            (Some(custom), false) => {
                format!("{BRANCH_SUMMARY_PROMPT}\n\nAdditional focus: {custom}")
            }
            (None, _) => BRANCH_SUMMARY_PROMPT.to_string(),
        }
    }
}

fn is_summary_entry(message: &MessageValue) -> bool {
    matches!(role_of(message), "branchSummary" | "compactionSummary")
}

/// Select messages for a branch summary, newest first under `token_budget`.
///
/// File operations are collected from every summary entry first, even those
/// outside the budget, so nested branch summaries stay cumulative.
pub fn prepare_branch_entries(messages: &[MessageValue], token_budget: usize) -> BranchPreparation {
    let mut file_ops = FileOpsTracker::new();
    for message in messages {
        if !is_summary_entry(message)
            || message.get("fromHook").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        if let Some(details) = message.get("details") {
            file_ops.merge_previous_details(&super::details_from_json(details));
        }
    }

    let mut kept = Vec::new();
    let mut total_tokens = 0;
    for message in messages.iter().rev() {
        file_ops.extract_from_message(message);
        let tokens = estimate_message_tokens(message);
        if token_budget > 0 && total_tokens + tokens > token_budget {
            // A prior summary is important context; allow it when it still fits
            // comfortably, then stop walking.
            if is_summary_entry(message) && (total_tokens as f64) < token_budget as f64 * 0.9 {
                kept.insert(0, message.clone());
                total_tokens += tokens;
            }
            break;
        }
        kept.insert(0, message.clone());
        total_tokens += tokens;
    }

    BranchPreparation {
        messages: kept,
        file_ops,
        total_tokens,
    }
}

/// Generate a branch summary with default options and an injected summarizer.
pub async fn generate_branch_summary<F, Fut>(
    messages: &[MessageValue],
    summarize_fn: F,
) -> BranchSummaryResult
where
    F: Fn(Vec<MessageValue>) -> Fut,
    Fut: Future<Output = Result<String, String>>,
{
    generate_branch_summary_with_options(messages, &BranchSummaryOptions::default(), summarize_fn)
        .await
}

/// Generate a branch summary with explicit options and an injected summarizer.
pub async fn generate_branch_summary_with_options<F, Fut>(
    messages: &[MessageValue],
    options: &BranchSummaryOptions,
    summarize_fn: F,
) -> BranchSummaryResult
where
    F: Fn(Vec<MessageValue>) -> Fut,
    Fut: Future<Output = Result<String, String>>,
{
    let token_budget = options
        .context_window
        .saturating_sub(options.reserve_tokens);
    let preparation = prepare_branch_entries(messages, token_budget);
    if preparation.messages.is_empty() {
        return BranchSummaryResult {
            summary: Some("No content to summarize".to_string()),
            ..BranchSummaryResult::default()
        };
    }

    let text = match summarize_fn(preparation.messages.clone()).await {
        Ok(text) => text,
        Err(error) => {
            return BranchSummaryResult {
                error: Some(error),
                ..BranchSummaryResult::default()
            };
        }
    };
    let text = if text.trim().is_empty() {
        "No summary generated"
    } else {
        text.as_str()
    };

    let lists = preparation.file_ops.compute_file_lists();
    let mut summary = format!("{BRANCH_SUMMARY_PREAMBLE}{text}");
    summary.push_str(&format_file_operations(
        &lists.read_files,
        &lists.modified_files,
    ));

    BranchSummaryResult {
        summary: Some(summary),
        read_files: lists.read_files,
        modified_files: lists.modified_files,
        aborted: false,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn user(text: &str) -> MessageValue {
        json!({"role": "user", "content": text})
    }

    fn constant_summarizer(
        text: &'static str,
    ) -> impl Fn(Vec<MessageValue>) -> std::future::Ready<Result<String, String>> {
        move |_| std::future::ready(Ok(text.to_string()))
    }

    #[test]
    fn prepare_branch_entries_walks_newest_first() {
        let messages = vec![user("old"), user("middle"), user("new")];
        let preparation = prepare_branch_entries(&messages, 0);
        assert_eq!(preparation.messages, messages);
        assert_eq!(preparation.total_tokens, 4);
    }

    #[test]
    fn prepare_branch_entries_respects_budget() {
        let messages = vec![user("aaaa"), user("bbbb"), user("cccc")];
        let preparation = prepare_branch_entries(&messages, 1);
        assert_eq!(preparation.messages, vec![user("cccc")]);
        assert_eq!(preparation.total_tokens, 1);
    }

    #[test]
    fn prepare_branch_entries_keeps_prior_summary_file_lists() {
        let summary = json!({
            "role": "branchSummary",
            "summary": "earlier branch",
            "details": {"readFiles": ["a.rs"], "modifiedFiles": ["b.rs"]}
        });
        let preparation = prepare_branch_entries(&[user("new"), summary], 10_000);
        let lists = preparation.file_ops.compute_file_lists();
        assert_eq!(lists.read_files, vec!["a.rs".to_string()]);
        assert_eq!(lists.modified_files, vec!["b.rs".to_string()]);
    }

    #[tokio::test]
    async fn generate_branch_summary_prepends_preamble_and_appends_files() {
        let messages = vec![
            user("please edit a.rs"),
            json!({
                "role": "assistant",
                "content": [{"type": "toolCall", "id": "t1", "name": "edit", "arguments": {"path": "a.rs"}}],
                "stopReason": "toolUse"
            }),
        ];
        let result =
            generate_branch_summary(&messages, constant_summarizer("The branch details")).await;
        let summary = result.summary.unwrap_or_default();
        assert!(summary.starts_with(BRANCH_SUMMARY_PREAMBLE));
        assert!(summary.contains("The branch details"));
        assert!(summary.contains("<modified-files>\na.rs\n</modified-files>"));
        assert_eq!(result.modified_files, vec!["a.rs".to_string()]);
        assert!(!result.aborted);
        assert_eq!(result.error, None);
    }

    #[tokio::test]
    async fn generate_branch_summary_handles_empty_and_errors() {
        let empty = generate_branch_summary(&[], constant_summarizer("x")).await;
        assert_eq!(empty.summary.as_deref(), Some("No content to summarize"));
        let failing = |_messages: Vec<MessageValue>| async { Err("offline".to_string()) };
        let failed = generate_branch_summary(&[user("hello")], failing).await;
        assert_eq!(failed.error.as_deref(), Some("offline"));
        assert_eq!(failed.summary, None);
    }

    #[tokio::test]
    async fn generate_branch_summary_with_options_honors_budget() {
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let summarizer = move |messages: Vec<MessageValue>| {
            let recorder = recorder.clone();
            async move {
                let mut calls = recorder.lock().map_err(|_| "poisoned".to_string())?;
                calls.push(messages.len());
                drop(calls);
                Ok("ok".to_string())
            }
        };
        let messages = vec![user("aaaa"), user("bbbb"), user("cccc")];
        let options = BranchSummaryOptions {
            context_window: 5,
            reserve_tokens: 4,
            custom_instructions: Some("focus on edits".to_string()),
            ..BranchSummaryOptions::default()
        };
        let _result = generate_branch_summary_with_options(&messages, &options, summarizer).await;
        let calls = match seen.lock() {
            Ok(guard) => guard.clone(),
            Err(_) => Vec::new(),
        };
        assert_eq!(calls, vec![1]);
        assert!(options.instructions().contains("focus on edits"));
    }
}
