use std::sync::LazyLock;

use regex::{RegexSet, RegexSetBuilder};

use crate::types::{AssistantMessage, StopReason};

const NON_RETRYABLE_PATTERNS: &[&str] = &[
    r"GoUsageLimitError",
    r"FreeUsageLimitError",
    r"Monthly usage limit reached",
    r"available balance",
    r"insufficient_quota",
    r"out of budget",
    r"quota exceeded",
    r"billing",
    r"subscription_sharing_usage_limit_exceeded",
    r"subscription_sharing_workplace_individual_usage_limit_exceeded",
    r#""code"\s*:\s*"?1308(?:[^0-9]|$)"#,
];

const RETRYABLE_PATTERNS: &[&str] = &[
    r"overloaded",
    r"server_busy",
    r"servers are currently busy",
    r"currently experiencing high demand",
    r"model is at capacity",
    r"admission ceiling",
    r"rate.?limit",
    r"too many requests",
    r"rate increased too quickly",
    r"(?:^|[^a-z0-9])(?:429|500|502|503|504|520|524|525)(?:[^a-z0-9]|$)",
    r"service.?(?:is.?)?(?:temporarily.?)?unavailable",
    r"provider.?(?:is.?)?temporarily.?unavailable",
    r"server.?error",
    r"internal.?error",
    r"provider.?returned.?(?:an.?)?error",
    r#""type"\s*:\s*"api_error""#,
    r"exceeded request buffer limit while retrying upstream",
    r"unable to forward upstream",
    r"upstream.?request.?failed",
    r"upstream_stream_read_error",
    r"stream was interrupted",
    r"http/?2 stream failed",
    r"network.?error",
    r"connection.?error",
    r"connection.?refused",
    r"connection.?lost",
    r"connection.?closed",
    r"other side closed",
    r"fetch failed",
    r"request to \S+ failed, reason:",
    r"getaddrinfo",
    r"(?:^|[^a-z0-9_])(?:ECONNRESET|ECONNABORTED|ECONNREFUSED|ETIMEDOUT|EPIPE)(?:[^a-z0-9_]|$)",
    r"connection reset by peer",
    r"ENOTFOUND",
    r"EAI_AGAIN",
    r"upstream.?connect",
    r"reset before headers",
    r"socket hang up",
    r"socket connection was closed",
    r"timed? out",
    r"timeout",
    r"terminated",
    r"websocket.?closed",
    r"websocket.?error",
    r"websocket.?connection.?limit.?reached",
    r"ended without",
    r"stream ended before message_stop",
    r"stream ended before a terminal response event",
    r"http2 request did not get a response",
    r"provider finish_reason: error",
    r"provider stopped with: error",
    r"returned an empty response",
    r"stopped before completing the response",
    r"stream disconnected before completion",
    r"cut short after the reply had begun",
    r"ended the response before it was complete",
    r"connection to the provider was reset",
    r"reported a failure while generating the response",
    r"pending stream has been canceled",
    r"retry delay",
    r"you can retry your request",
    r"try your request again",
    r"please retry your request",
    r"repeat the request later",
    r"ResourceExhausted",
    r"subscription_sharing_usage_unavailable",
    r"subscription_sharing_user_unavailable",
];

// INFALLIBLE: static regex patterns validated by the retry classifier unit tests.
static NON_RETRYABLE_LIMIT: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSetBuilder::new(NON_RETRYABLE_PATTERNS)
        .case_insensitive(true)
        .build()
        .expect("valid static non-retryable patterns")
});

// INFALLIBLE: static regex patterns validated by the retry classifier unit tests.
static RETRYABLE: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSetBuilder::new(RETRYABLE_PATTERNS)
        .case_insensitive(true)
        .build()
        .expect("valid static retryable patterns")
});

/// Returns true if a provider error message represents a transient failure that should be retried.
pub fn is_retryable_provider_error(error_message: &str) -> bool {
    if NON_RETRYABLE_LIMIT.is_match(error_message) {
        return false;
    }
    RETRYABLE.is_match(error_message)
}

/// Returns true if an assistant message failed with a retryable provider error.
pub fn is_retryable_assistant_error(message: &AssistantMessage) -> bool {
    message.stop_reason == StopReason::Error
        && message
            .error_message
            .as_deref()
            .is_some_and(is_retryable_provider_error)
}

#[cfg(test)]
mod tests {
    use super::is_retryable_provider_error;

    #[test]
    fn matches_rate_increased_too_quickly_and_finish_reason_error() {
        assert!(is_retryable_provider_error(
            "Your request rate increased too quickly"
        ));
        assert!(is_retryable_provider_error(
            "Request rate increased too quickly. To ensure system stability, please adjust your client logic."
        ));
        assert!(is_retryable_provider_error("Provider finish_reason: error"));
        assert!(is_retryable_provider_error("Provider stopped with: error"));
        assert!(!is_retryable_provider_error(
            "Provider finish_reason: content_filter"
        ));
        assert!(!is_retryable_provider_error(
            "insufficient_quota: rate limit"
        ));
    }
}
