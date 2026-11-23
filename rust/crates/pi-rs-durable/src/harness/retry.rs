use regex::RegexSet;
use std::sync::LazyLock;
static LIMIT: LazyLock<RegexSet> = LazyLock::new(|| {
    // INFALLIBLE: these expressions are fixed upstream provider error patterns.
    RegexSet::new([
        r#"(?i)GoUsageLimitError"#,
        r#"(?i)FreeUsageLimitError"#,
        r#"(?i)Monthly usage limit reached"#,
        r#"(?i)available balance"#,
        r#"(?i)insufficient_quota"#,
        r#"(?i)out of budget"#,
        r#"(?i)quota exceeded"#,
        r#"(?i)billing"#,
        r#"(?i)subscription_sharing_usage_limit_exceeded"#,
        r#"(?i)subscription_sharing_workplace_individual_usage_limit_exceeded"#,
        r#"(?i)"code"\s*:\s*"?1308(?:$|[^0-9])"#,
    ])
    .expect("valid provider error expressions")
});
static TRANSIENT: LazyLock<RegexSet> = LazyLock::new(|| {
    // INFALLIBLE: these expressions are fixed upstream provider error patterns.
    RegexSet::new([
        r#"(?i)overloaded"#,
        r#"(?i)server_busy"#,
        r#"(?i)servers are currently busy"#,
        r#"(?i)currently experiencing high demand"#,
        r#"(?i)model is at capacity"#,
        r#"(?i)admission ceiling"#,
        r#"(?i)rate.?limit"#,
        r#"(?i)too many requests"#,
        r#"(?i)(?:^|[^a-z0-9])(?:429|500|502|503|504|520|524|525)(?:$|[^a-z0-9])"#,
        r#"(?i)service.?(?:is.?)?(?:temporarily.?)?unavailable"#,
        r#"(?i)provider.?(?:is.?)?temporarily.?unavailable"#,
        r#"(?i)server.?error"#,
        r#"(?i)internal.?error"#,
        r#"(?i)provider.?returned.?(?:an.?)?error"#,
        r#"(?i)"type"\s*:\s*"api_error""#,
        r#"(?i)exceeded request buffer limit while retrying upstream"#,
        r#"(?i)unable to forward upstream"#,
        r#"(?i)upstream.?request.?failed"#,
        r#"(?i)upstream_stream_read_error"#,
        r#"(?i)stream was interrupted"#,
        r#"(?i)http/?2 stream failed"#,
        r#"(?i)network.?error"#,
        r#"(?i)connection.?error"#,
        r#"(?i)connection.?refused"#,
        r#"(?i)connection.?lost"#,
        r#"(?i)connection.?closed"#,
        r#"(?i)other side closed"#,
        r#"(?i)fetch failed"#,
        r#"(?i)request to \S+ failed, reason:"#,
        r#"(?i)getaddrinfo"#,
        r#"(?i)(?:^|[^a-z0-9_])(?:ECONNRESET|ECONNABORTED|ECONNREFUSED|ETIMEDOUT|EPIPE)(?:$|[^a-z0-9_])"#,
        r#"(?i)connection reset by peer"#,
        r#"(?i)ENOTFOUND"#,
        r#"(?i)EAI_AGAIN"#,
        r#"(?i)upstream.?connect"#,
        r#"(?i)reset before headers"#,
        r#"(?i)socket hang up"#,
        r#"(?i)socket connection was closed"#,
        r#"(?i)timed? out"#,
        r#"(?i)timeout"#,
        r#"(?i)terminated"#,
        r#"(?i)websocket.?closed"#,
        r#"(?i)websocket.?error"#,
        r#"(?i)websocket.?connection.?limit.?reached"#,
        r#"(?i)ended without"#,
        r#"(?i)stream ended before message_stop"#,
        r#"(?i)stream ended before a terminal response event"#,
        r#"(?i)http2 request did not get a response"#,
        r#"(?i)provider finish_reason: error"#,
        r#"(?i)returned an empty response"#,
        r#"(?i)stopped before completing the response"#,
        r#"(?i)stream disconnected before completion"#,
        r#"(?i)cut short after the reply had begun"#,
        r#"(?i)ended the response before it was complete"#,
        r#"(?i)connection to the provider was reset"#,
        r#"(?i)reported a failure while generating the response"#,
        r#"(?i)pending stream has been canceled"#,
        r#"(?i)retry delay"#,
        r#"(?i)you can retry your request"#,
        r#"(?i)try your request again"#,
        r#"(?i)please retry your request"#,
        r#"(?i)repeat the request later"#,
        r#"(?i)ResourceExhausted"#,
        r#"(?i)subscription_sharing_usage_unavailable"#,
        r#"(?i)subscription_sharing_user_unavailable"#,
    ]).expect("valid provider error expressions")
});
pub(super) fn delay(settings: &serde_json::Value, error: &str, attempt: u64) -> Option<u64> {
    let policy = &settings["retry"];
    if policy["enabled"] == false
        || attempt > policy["maxRetries"].as_u64().unwrap_or(3)
        || LIMIT.is_match(error)
        || !TRANSIENT.is_match(error)
    {
        return None;
    }
    let base = policy["baseDelayMs"].as_u64().unwrap_or(2000);
    let cap = policy["maxAgentDelayMs"].as_u64().unwrap_or(60000);
    Some(
        base.saturating_mul(2u64.saturating_pow(attempt.saturating_sub(1).min(64) as u32))
            .min(cap),
    )
}
