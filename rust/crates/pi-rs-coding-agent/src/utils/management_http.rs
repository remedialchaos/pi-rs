//! Transport-level HTTP fetch with bounded retries, ported from `utils/management-http.ts`.
//! There is no management HTTP server here, only this GET helper for idempotent resources.
//! The network is injectable: tests pass a closure instead of using [`reqwest_fetch`].

use std::fmt;
use std::future::Future;
use std::time::{Duration, Instant};

pub const RETRYABLE_STATUS_CODES: [u16; 7] = [408, 425, 429, 500, 502, 503, 504];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// Time left for the overall budget (or the per-attempt budget when no overall budget exists).
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    Transport(String),
    Timeout,
    Cancelled,
    InvalidHeader(String),
}

impl fmt::Display for FetchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(message) => write!(formatter, "fetch failed: {message}"),
            Self::Timeout => write!(formatter, "fetch timed out"),
            Self::Cancelled => write!(formatter, "fetch cancelled"),
            Self::InvalidHeader(name) => write!(formatter, "invalid header: {name}"),
        }
    }
}

impl std::error::Error for FetchError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FetchRetryOptions {
    /// Additional attempts after the first. `None` means upstream's default of 2.
    pub max_retries: Option<usize>,
    pub retry_on_status: bool,
    pub timeout_ms: Option<u64>,
    pub attempt_timeout_ms: Option<u64>,
}

impl Default for FetchRetryOptions {
    fn default() -> Self {
        Self {
            max_retries: None,
            retry_on_status: true,
            timeout_ms: None,
            attempt_timeout_ms: None,
        }
    }
}

fn is_retryable_status(status: u16) -> bool {
    RETRYABLE_STATUS_CODES.contains(&status)
}

/// Retry transport failures and (optionally) transient HTTP statuses, with no backoff, matching
/// upstream. `timeoutMs` is an overall deadline shared by attempts; `attemptTimeoutMs` applies to
/// each attempt and can be retried. Cancellation is terminal.
pub async fn fetch_with_retry<F, Fut>(
    url: &str,
    headers: &[(String, String)],
    options: FetchRetryOptions,
    fetch: F,
) -> Result<FetchResponse, FetchError>
where
    F: Fn(FetchRequest) -> Fut,
    Fut: Future<Output = Result<FetchResponse, FetchError>>,
{
    let max_retries = options.max_retries.unwrap_or(2);
    let deadline = options
        .timeout_ms
        .filter(|ms| *ms > 0)
        .map(|ms| Instant::now() + Duration::from_millis(ms));
    let attempt_timeout = options.attempt_timeout_ms.filter(|ms| *ms > 0);
    let mut attempt = 0usize;

    loop {
        let request_timeout = match deadline {
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(FetchError::Timeout);
                }
                Some(remaining)
            }
            None => attempt_timeout.map(Duration::from_millis),
        };
        let request = FetchRequest {
            url: url.to_string(),
            headers: headers.to_vec(),
            timeout: request_timeout,
        };

        let result = match deadline {
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(FetchError::Timeout);
                }
                match tokio::time::timeout(remaining, fetch(request)).await {
                    Ok(result) => result,
                    Err(_) => Err(FetchError::Timeout),
                }
            }
            None => fetch(request).await,
        };

        match result {
            Ok(response) => {
                let retry = options.retry_on_status
                    && is_retryable_status(response.status)
                    && attempt < max_retries;
                if retry {
                    attempt += 1;
                    continue;
                }
                return Ok(response);
            }
            Err(error) => {
                if error == FetchError::Cancelled || attempt >= max_retries {
                    return Err(error);
                }
                // An overall timeout is terminal; a per-attempt timeout retries.
                if error == FetchError::Timeout && deadline.is_some() {
                    return Err(error);
                }
                attempt += 1;
            }
        }
    }
}

/// Real transport used by production callers.
pub async fn reqwest_fetch(request: FetchRequest) -> Result<FetchResponse, FetchError> {
    let mut builder = reqwest::Client::builder();
    if let Some(timeout) = request.timeout {
        builder = builder.timeout(timeout);
    }
    let client = builder
        .build()
        .map_err(|error| FetchError::Transport(error.to_string()))?;
    let mut call = client.get(&request.url);
    for (name, value) in &request.headers {
        let name = reqwest::header::HeaderName::try_from(name.as_str())
            .map_err(|_| FetchError::InvalidHeader(name.clone()))?;
        call = call.header(name, value.as_str());
    }
    let response = call.send().await.map_err(|error| {
        if error.is_timeout() {
            FetchError::Timeout
        } else {
            FetchError::Transport(error.to_string())
        }
    })?;
    let status = response.status().as_u16();
    let body = response
        .bytes()
        .await
        .map_err(|error| FetchError::Transport(error.to_string()))?
        .to_vec();
    Ok(FetchResponse { status, body })
}

pub async fn fetch_with_retry_http(
    url: &str,
    headers: &[(String, String)],
    options: FetchRetryOptions,
) -> Result<FetchResponse, FetchError> {
    fetch_with_retry(url, headers, options, reqwest_fetch).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn response(status: u16) -> FetchResponse {
        FetchResponse {
            status,
            body: Vec::new(),
        }
    }

    #[test]
    fn retryable_status_set_matches_upstream() {
        for status in RETRYABLE_STATUS_CODES {
            assert!(is_retryable_status(status));
        }
        assert!(!is_retryable_status(200));
        assert!(!is_retryable_status(404));
    }

    #[tokio::test]
    async fn transport_failures_are_retried_twice_by_default() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let result = fetch_with_retry(
            "https://example.invalid",
            &[],
            FetchRetryOptions::default(),
            move |_| {
                let count = counter.fetch_add(1, Ordering::SeqCst);
                async move {
                    if count < 2 {
                        Err(FetchError::Transport("connect failed".to_string()))
                    } else {
                        Ok(response(200))
                    }
                }
            },
        )
        .await;
        assert_eq!(result, Ok(response(200)));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retryable_status_is_retried() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let result = fetch_with_retry(
            "https://example.invalid",
            &[],
            FetchRetryOptions::default(),
            move |_| {
                let count = counter.fetch_add(1, Ordering::SeqCst);
                async move { Ok(response(if count == 0 { 503 } else { 200 })) }
            },
        )
        .await;
        assert_eq!(result, Ok(response(200)));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn retry_can_be_disabled_for_statuses() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let result = fetch_with_retry(
            "https://example.invalid",
            &[],
            FetchRetryOptions {
                retry_on_status: false,
                ..FetchRetryOptions::default()
            },
            move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                async { Ok(response(503)) }
            },
        )
        .await;
        assert_eq!(result, Ok(response(503)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn max_retries_zero_means_one_call() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let result = fetch_with_retry(
            "https://example.invalid",
            &[],
            FetchRetryOptions {
                max_retries: Some(0),
                ..FetchRetryOptions::default()
            },
            move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                async { Err(FetchError::Transport("down".to_string())) }
            },
        )
        .await;
        assert_eq!(result, Err(FetchError::Transport("down".to_string())));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancellation_is_terminal() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let result = fetch_with_retry(
            "https://example.invalid",
            &[],
            FetchRetryOptions::default(),
            move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                async { Err(FetchError::Cancelled) }
            },
        )
        .await;
        assert_eq!(result, Err(FetchError::Cancelled));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn overall_timeout_is_terminal() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let result = fetch_with_retry(
            "https://example.invalid",
            &[],
            FetchRetryOptions {
                timeout_ms: Some(10),
                ..FetchRetryOptions::default()
            },
            move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                async {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    Ok(response(200))
                }
            },
        )
        .await;
        assert_eq!(result, Err(FetchError::Timeout));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn attempt_timeout_is_retried() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let result = fetch_with_retry(
            "https://example.invalid",
            &[],
            FetchRetryOptions {
                attempt_timeout_ms: Some(10),
                ..FetchRetryOptions::default()
            },
            move |request| {
                let count = counter.fetch_add(1, Ordering::SeqCst);
                async move {
                    let delay = if count == 0 { 200 } else { 0 };
                    match request.timeout {
                        Some(timeout) if delay > timeout.as_millis() as u64 => {
                            tokio::time::sleep(timeout).await;
                            Err(FetchError::Timeout)
                        }
                        _ => Ok(response(200)),
                    }
                }
            },
        )
        .await;
        assert_eq!(result, Ok(response(200)));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn non_retryable_status_is_returned_immediately() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let result = fetch_with_retry(
            "https://example.invalid",
            &[],
            FetchRetryOptions::default(),
            move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                async { Ok(response(404)) }
            },
        )
        .await;
        assert_eq!(result, Ok(response(404)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
