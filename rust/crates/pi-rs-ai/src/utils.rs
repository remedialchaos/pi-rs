use std::fmt::Write;
use std::sync::LazyLock;

use pi_rs_schema::{
    CoercingValidator, CompileOptions, UnresolvedRefs, compile_coercing, formats::formats,
};
use regex::{Regex, RegexSet};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::types::{AssistantMessage, StopReason, Tool, ToolCall};

pub const OVERFLOW_PATTERNS: &[&str] = &[
    r"prompt (?:is )?too long",
    r"prompt exceeds max length",
    r"request_too_large",
    r"input is too long for requested model",
    r"exceeds the context window",
    r"exceeds (?:the )?(?:model'?s )?maximum context length(?: of [0-9,]+ tokens?|\s*\([0-9,]+\))",
    r"context window exceeded",
    r"input token count.*exceeds the maximum",
    r"maximum prompt length is [0-9]+",
    r"reduce the length of the messages",
    r"maximum context length is [0-9]+ tokens",
    r"exceeds (?:the )?maximum allowed input length of [0-9,]+ tokens?",
    r"input \([0-9]+ tokens\) is longer than the model'?s context length \([0-9]+ tokens\)",
    r"exceeds the limit of [0-9]+",
    r"exceeds the available context size",
    r"greater than the context length",
    r"context window exceeds limit",
    r"exceeded model token limit",
    r"too large for model with [0-9]+ maximum context length",
    r"prompt has [0-9,]+ tokens?, but the configured context size is [0-9,]+ tokens?",
    r"model_context_window_exceeded",
    r"prompt too long; exceeded (?:max )?context length",
    r"range of input length should be",
    r"context[_ ]length[_ ]exceeded",
    r"too many tokens",
    r"token limit exceeded",
    r"^(?:[A-Za-z0-9_ -]+ API error \(413\):|413(?:\s|:|$))",
];
// INFALLIBLE: static regex patterns validated by the context-overflow tests.
static OVERFLOW: LazyLock<RegexSet> = LazyLock::new(|| {
    regex::RegexSetBuilder::new(OVERFLOW_PATTERNS)
        .case_insensitive(true)
        .build()
        .expect("valid static overflow patterns")
});
static NON_OVERFLOW: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSet::new([
        r"(?i)^(Throttling error|Service unavailable):",
        r"(?i)rate limit",
        r"(?i)too many requests",
    ])
    .expect("valid static non-overflow patterns")
});
static CEREBRAS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^4(?:00|13)\s*(?:status code)?\s*\(no body\)")
        .expect("valid static Cerebras error pattern")
});

pub fn is_context_overflow(message: &AssistantMessage, context_window: Option<u64>) -> bool {
    if message.stop_reason == StopReason::Error
        && let Some(error) = message.error_message.as_deref()
        && !NON_OVERFLOW.is_match(error)
        && (OVERFLOW.is_match(error) || message.provider == "cerebras" && CEREBRAS.is_match(error))
    {
        return true;
    }
    if let Some(window) = context_window.filter(|n| *n > 0) {
        let input = message.usage.input + message.usage.cache_read;
        let window = window as f64;
        return match message.stop_reason {
            StopReason::Stop => input > window,
            StopReason::Length => message.usage.output == 0.0 && input >= window * 0.99,
            _ => false,
        };
    }
    false
}
pub fn is_recoverable_length(message: &AssistantMessage, desired_max_output: u64) -> bool {
    message.stop_reason == StopReason::Length
        && desired_max_output > 0
        && message.usage.output < desired_max_output as f64
}

/// Rust UTF-8 strings cannot contain a lone surrogate; use this at UTF-16 boundaries.
pub fn sanitize_surrogates(input: impl IntoIterator<Item = u16>) -> String {
    char::decode_utf16(input)
        .filter_map(std::result::Result::ok)
        .collect()
}
pub fn headers_to_record(
    headers: &reqwest::header::HeaderMap,
) -> indexmap::IndexMap<String, String> {
    headers
        .iter()
        .filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned())))
        .collect()
}
pub fn with_cause_detail(error: &(dyn std::error::Error + 'static)) -> String {
    let message = error.to_string();
    let mut source = error.source();
    let mut last = None;
    for _ in 0..5 {
        if let Some(e) = source {
            last = Some(e);
            source = e.source();
        } else {
            break;
        }
    }
    if let Some(last) = last {
        let detail = last.to_string();
        let detail = detail.trim();
        if !detail.is_empty() && !message.contains(detail) {
            return if message.is_empty() {
                detail.into()
            } else {
                format!("{message}: {detail}")
            };
        }
    }
    message
}

pub struct ToolValidator(CoercingValidator);
impl ToolValidator {
    pub fn compile(tool: &Tool) -> Result<Self> {
        static OPTIONS: LazyLock<CompileOptions> = LazyLock::new(|| {
            CompileOptions::new()
                .with_formats(formats())
                .with_unresolved_refs(UnresolvedRefs::Reject)
        });
        compile_coercing(&tool.parameters, &OPTIONS)
            .map(Self)
            .map_err(|e| Error::Protocol(e.to_string()))
    }
    pub fn validate(&self, call: &ToolCall) -> Result<Value> {
        if let Some(error) = call.arguments_error.as_deref().filter(|s| !s.is_empty()) {
            return Err(invalid_arguments(&call.name, error));
        }
        let input = Value::Object(call.arguments.clone());
        match self.0.parse(&pi_rs_schema::Value::from(&input)) {
            Ok(parsed) => Value::try_from(parsed).map_err(|e| Error::Protocol(e.to_string())),
            Err(errors) => {
                let mut report = format!("Validation failed for tool \"{}\":", call.name);
                for error in errors {
                    let path = if error.path.is_empty() {
                        "root".into()
                    } else {
                        error
                            .path
                            .trim_start_matches('/')
                            .split('/')
                            .map(|p| p.replace("~1", "/").replace("~0", "~"))
                            .collect::<Vec<_>>()
                            .join(".")
                    };
                    let _ = write!(report, "\n  - {path}: {}", error.message);
                }
                report.push_str("\n\nReceived arguments:\n");
                report.push_str(&serde_json::to_string_pretty(&input)?);
                Err(Error::Protocol(report))
            }
        }
    }
}
fn invalid_arguments(tool: &str, error: &str) -> Error {
    Error::Protocol(format!("Invalid arguments for tool \"{tool}\": {error}"))
}
pub fn validate_tool_arguments(tool: &Tool, call: &ToolCall) -> Result<Value> {
    if let Some(error) = call.arguments_error.as_deref().filter(|s| !s.is_empty()) {
        return Err(invalid_arguments(&call.name, error));
    }
    ToolValidator::compile(tool)?.validate(call)
}
pub fn validate_tool_call(tools: &[Tool], call: &ToolCall) -> Result<Value> {
    let tool = tools
        .iter()
        .find(|t| t.name == call.name)
        .ok_or_else(|| Error::Protocol(format!("Tool \"{}\" not found", call.name)))?;
    validate_tool_arguments(tool, call)
}
