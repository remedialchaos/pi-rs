//! The ambient `fetch`, `Response` and `TextDecoder` the upstream TypeScript runs against,
//! replaced by an injectable function type and small value types, with `reqwest` as the platform
//! implementation. Error kinds keep the distinction the transports rely on: a network failure
//! (upstream: `TypeError`) is retryable, other failures are not.

use std::pin::Pin;

use futures_util::{Stream, StreamExt};
use http::HeaderMap;

use crate::abort::AbortSignal;

/// A response body as a stream of byte chunks. Dropping it cancels the response, like
/// `response.body.cancel()` upstream.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, McpFetchError>> + Send>>;

/// A future the fetch function returns.
pub type FetchFuture =
    Pin<Box<dyn Future<Output = Result<McpFetchResponse, McpFetchError>> + Send>>;

/// One `fetch` call: the URL plus method, headers, body and signal of `RequestInit`.
pub struct McpFetchRequest {
    pub url: String,
    pub method: http::Method,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    /// Aborts the request, like `RequestInit#signal`.
    pub signal: Option<AbortSignal>,
}

impl std::fmt::Debug for McpFetchRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpFetchRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("headers", &self.headers)
            .field("body", &self.body)
            .finish()
    }
}

/// What a failed `fetch` reports. Network failures correspond to the `TypeError` the Web (and
/// undici) throw; they are the retryable kind.
#[derive(Debug, Clone, thiserror::Error)]
pub enum McpFetchError {
    #[error("{0}")]
    Network(String),
    #[error("{0}")]
    Timeout(String),
    #[error("{0}")]
    Other(String),
}

/// A `fetch` response: status, headers, and the body as a stream, absent when there is none.
pub struct McpFetchResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Option<ByteStream>,
}

impl McpFetchResponse {
    pub fn new(status: u16, headers: HeaderMap, body: Option<ByteStream>) -> Self {
        Self {
            status,
            headers,
            body,
        }
    }

    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// One header, lowercased like `Headers#get`.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    /// The media type: the `content-type` up to its first `;`, trimmed and lowercased.
    pub fn content_type(&self) -> Option<String> {
        self.header("content-type").map(|value| {
            value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
        })
    }

    /// Take the body out and drop it, cancelling the response, like a `body.cancel()` that cannot
    /// fail. Never waits.
    pub fn discard(&mut self) {
        self.body.take();
    }
}

/// Builds a `HeaderMap` from lowercased name/value pairs, for tests and callers that assemble
/// headers by hand.
pub fn header_map<'a, P>(pairs: impl IntoIterator<Item = P>) -> HeaderMap
where
    P: std::borrow::Borrow<(&'a str, &'a str)>,
{
    let mut headers = HeaderMap::new();
    for (name, value) in pairs.into_iter().map(|pair| *pair.borrow()) {
        let name = http::HeaderName::from_bytes(name.as_bytes()).unwrap_or(http::header::ACCEPT);
        let value =
            http::HeaderValue::from_str(value).unwrap_or(http::HeaderValue::from_static(""));
        headers.insert(name, value);
    }
    headers
}

/// The platform `fetch` over `reqwest`. Follows redirects like the Web one and streams the body.
pub fn platform_fetch(request: McpFetchRequest) -> FetchFuture {
    Box::pin(async move {
        static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
        let client = CLIENT.get_or_init(|| reqwest::Client::builder().build().unwrap_or_default());
        let method = reqwest::Method::from_bytes(request.method.as_str().as_bytes())
            .unwrap_or(reqwest::Method::GET);
        let mut out = client.request(method, &request.url).headers(header_map(
            request
                .headers
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str()))
                .collect::<Vec<_>>(),
        ));
        if let Some(body) = &request.body {
            out = out.body(body.clone());
        }
        let mut pending = Box::pin(out.send());
        let response = if let Some(signal) = &request.signal {
            tokio::select! {
                biased;
                _ = signal.clone().wait_aborted() => {
                    return Err(McpFetchError::Timeout("The operation was aborted".to_string()));
                }
                response = &mut pending => response,
            }
        } else {
            pending.await
        }
        .map_err(|error| McpFetchError::Network(error.to_string()))?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let stream: ByteStream = Box::pin(response.bytes_stream().map(|chunk| {
            chunk
                .map(|bytes| bytes.to_vec())
                .map_err(|error| McpFetchError::Network(error.to_string()))
        }));
        // The signal aborts the whole request, body streaming included, like the Web `fetch`
        // upstream calls with a `signal`.
        let stream = match request.signal.clone() {
            Some(signal) => {
                if signal.aborted() {
                    None
                } else {
                    Some(Box::pin(AbortableBody::new(stream, signal)) as ByteStream)
                }
            }
            None => Some(stream),
        };
        Ok(McpFetchResponse {
            status,
            headers,
            body: stream,
        })
    })
}

/// A response body that stops, and reports the abort, when its request's signal fires.
struct AbortableBody {
    inner: ByteStream,
    signal: crate::abort::AbortSignal,
    wait: Pin<Box<dyn Future<Output = String> + Send>>,
}

impl AbortableBody {
    fn new(inner: ByteStream, signal: crate::abort::AbortSignal) -> Self {
        let owned_signal = signal.clone();
        let wait = owned_signal.wait_aborted();
        Self {
            inner,
            signal,
            wait: Box::pin(wait),
        }
    }
}

impl Stream for AbortableBody {
    type Item = Result<Vec<u8>, McpFetchError>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = &mut *self;
        if this.signal.aborted() {
            return std::task::Poll::Ready(Some(Err(McpFetchError::Timeout(
                "The operation was aborted".to_string(),
            ))));
        }
        match this.inner.as_mut().poll_next(cx) {
            std::task::Poll::Ready(item) => std::task::Poll::Ready(item),
            std::task::Poll::Pending => {
                // Wake when either the body or the signal makes progress.
                match this.wait.as_mut().poll(cx) {
                    std::task::Poll::Pending => std::task::Poll::Pending,
                    std::task::Poll::Ready(_) => std::task::Poll::Ready(Some(Err(
                        McpFetchError::Timeout("The operation was aborted".to_string()),
                    ))),
                }
            }
        }
    }
}

/// An incremental UTF-8 decoder with the `TextDecoder` defaults: never fails, replaces invalid
/// sequences with U+FFFD, and carries an incomplete trailing sequence across chunks.
#[derive(Default)]
pub struct StreamingDecoder {
    pending: Vec<u8>,
}
impl StreamingDecoder {
    /// Decodes the complete characters of `chunk`, keeping a truncated trailing sequence for the
    /// next chunk, like `TextDecoder#decode(chunk, { stream: true })`.
    pub fn push(&mut self, chunk: &[u8]) -> String {
        self.pending.extend_from_slice(chunk);
        let mut out = String::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(text) => {
                    out.push_str(text);
                    self.pending.clear();
                    return out;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    // The valid prefix is valid UTF-8, so this never replaces anything.
                    out.push_str(&String::from_utf8_lossy(&self.pending[..valid]));
                    match error.error_len() {
                        Some(invalid) => {
                            out.push('\u{FFFD}');
                            self.pending.drain(..valid + invalid);
                        }
                        None => {
                            // An incomplete trailing sequence: wait for more bytes.
                            self.pending.drain(..valid);
                            return out;
                        }
                    }
                }
            }
        }
    }

    /// Decodes what is left, replacing a still-truncated sequence with U+FFFD, like a final
    /// `TextDecoder#decode()`.
    pub fn finish(&mut self) -> String {
        let out = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        out
    }
}

/// Reads a response body as text, decoding chunks as they arrive. Once the body exceeds
/// `max_bytes`, the rest is cancelled (dropping the stream) and the text holds only the first
/// `max_bytes` bytes; a truncated trailing sequence is dropped, as the Web `TextDecoder` stream
/// decode upstream does. Returns the text and whether it was truncated.
pub async fn read_body_text(
    response: &mut McpFetchResponse,
    max_bytes: usize,
) -> Result<(String, bool), McpFetchError> {
    use futures_util::StreamExt;
    let Some(mut stream) = response.body.take() else {
        return Ok((String::new(), false));
    };
    let mut decoder = StreamingDecoder::default();
    let mut text = String::new();
    let mut remaining = max_bytes;
    loop {
        let chunk = stream
            .next()
            .await
            .transpose()
            .map_err(|error| McpFetchError::Network(error.to_string()))?;
        let Some(chunk) = chunk else {
            text.push_str(&decoder.finish());
            return Ok((text, false));
        };
        if chunk.len() > remaining {
            // Dropping the stream cancels the response.
            text.push_str(&decoder.push(&chunk[..remaining]));
            return Ok((text, true));
        }
        remaining -= chunk.len();
        text.push_str(&decoder.push(&chunk));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_across_chunk_boundaries_and_replaces_invalid_bytes() {
        let mut decoder = StreamingDecoder::default();
        let text = "a\u{E9}\u{20AC}\u{1F30D}end";
        let bytes = text.as_bytes();
        let mut decoded = String::new();
        for byte in bytes {
            decoded.push_str(&decoder.push(&[*byte]));
        }
        decoded.push_str(&decoder.finish());
        assert_eq!(decoded, text);
        assert_eq!(decoder.push(&[0xff, b'a']), "\u{FFFD}a");
    }
}
