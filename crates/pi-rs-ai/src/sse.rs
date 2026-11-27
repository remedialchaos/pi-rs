use std::borrow::Cow;
use std::collections::VecDeque;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};
use crate::http::abortable;

pub const MAX_LINE_BUFFER_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_EVENT_DATA_BYTES: usize = 32 * 1024 * 1024;
pub const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

fn intern_event_name(name: &str) -> Cow<'static, str> {
    match name {
        "" => Cow::Borrowed(""),
        "message" => Cow::Borrowed("message"),
        "ping" => Cow::Borrowed("ping"),
        "error" => Cow::Borrowed("error"),
        "message_start" => Cow::Borrowed("message_start"),
        "message_delta" => Cow::Borrowed("message_delta"),
        "message_stop" => Cow::Borrowed("message_stop"),
        "content_block_start" => Cow::Borrowed("content_block_start"),
        "content_block_delta" => Cow::Borrowed("content_block_delta"),
        "content_block_stop" => Cow::Borrowed("content_block_stop"),
        "response.created" => Cow::Borrowed("response.created"),
        "response.in_progress" => Cow::Borrowed("response.in_progress"),
        "response.completed" => Cow::Borrowed("response.completed"),
        "response.failed" => Cow::Borrowed("response.failed"),
        "response.incomplete" => Cow::Borrowed("response.incomplete"),
        "response.done" => Cow::Borrowed("response.done"),
        "response.output_item.added" => Cow::Borrowed("response.output_item.added"),
        "response.output_item.done" => Cow::Borrowed("response.output_item.done"),
        "response.content_part.added" => Cow::Borrowed("response.content_part.added"),
        "response.content_part.done" => Cow::Borrowed("response.content_part.done"),
        "response.output_text.delta" => Cow::Borrowed("response.output_text.delta"),
        "response.output_text.done" => Cow::Borrowed("response.output_text.done"),
        "response.function_call_arguments.delta" => {
            Cow::Borrowed("response.function_call_arguments.delta")
        }
        "response.function_call_arguments.done" => {
            Cow::Borrowed("response.function_call_arguments.done")
        }
        "response.reasoning_summary_text.delta" => {
            Cow::Borrowed("response.reasoning_summary_text.delta")
        }
        "response.reasoning_summary_text.done" => {
            Cow::Borrowed("response.reasoning_summary_text.done")
        }
        "response.reasoning_text.delta" => Cow::Borrowed("response.reasoning_text.delta"),
        "response.reasoning_text.done" => Cow::Borrowed("response.reasoning_text.done"),
        other => Cow::Owned(other.to_owned()),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SseEvent {
    pub name: Cow<'static, str>,
    pub data: String,
}

pub struct SseDecoder {
    pending: Vec<u8>,
    data: String,
    name: Cow<'static, str>,
    has_data: bool,
    skip_lf: bool,
    saw_first_line: bool,
    max_line_bytes: usize,
    max_event_data_bytes: usize,
}

impl Default for SseDecoder {
    fn default() -> Self {
        Self::with_limits(MAX_LINE_BUFFER_BYTES, MAX_EVENT_DATA_BYTES)
    }
}

impl SseDecoder {
    pub fn with_limits(max_line_bytes: usize, max_event_data_bytes: usize) -> Self {
        Self {
            pending: Vec::new(),
            data: String::new(),
            name: Cow::Borrowed(""),
            has_data: false,
            skip_lf: false,
            saw_first_line: false,
            max_line_bytes,
            max_event_data_bytes,
        }
    }

    fn dispatch(&mut self, queue: &mut VecDeque<SseEvent>) {
        if self.has_data {
            self.has_data = false;
            queue.push_back(SseEvent {
                name: std::mem::replace(&mut self.name, Cow::Borrowed("")),
                data: std::mem::take(&mut self.data),
            });
        } else {
            self.name = Cow::Borrowed("");
        }
    }

    fn line(&mut self, line: &[u8], queue: &mut VecDeque<SseEvent>) -> Result<()> {
        let line = if self.saw_first_line {
            line
        } else {
            self.saw_first_line = true;
            line.strip_prefix(b"\xef\xbb\xbf").unwrap_or(line)
        };
        if line.is_empty() {
            self.dispatch(queue);
            return Ok(());
        }
        let split = line.iter().position(|b| *b == b':').unwrap_or(line.len());
        let value = line.get(split + 1..).unwrap_or_default();
        let value = value.strip_prefix(b" ").unwrap_or(value);
        match &line[..split] {
            b"event" => {
                let decoded = String::from_utf8_lossy(value);
                self.name = intern_event_name(&decoded);
            }
            b"data" => {
                let decoded = String::from_utf8_lossy(value);
                let extra = usize::from(self.has_data) + decoded.len();
                if self.data.len().saturating_add(extra) > self.max_event_data_bytes {
                    return Err(Error::Protocol(format!(
                        "SSE event data exceeded limit of {} bytes",
                        self.max_event_data_bytes
                    )));
                }
                if self.has_data {
                    self.data.push('\n');
                }
                self.has_data = true;
                self.data.push_str(&decoded);
            }
            _ => {}
        }
        Ok(())
    }

    pub fn push(&mut self, mut bytes: &[u8], queue: &mut VecDeque<SseEvent>) -> Result<()> {
        if self.skip_lf && !bytes.is_empty() {
            self.skip_lf = false;
            bytes = bytes.strip_prefix(b"\n").unwrap_or(bytes);
        }
        while let Some(end) = bytes.iter().position(|b| matches!(b, b'\r' | b'\n')) {
            let separator = bytes[end];
            if self.pending.is_empty() {
                if end > self.max_line_bytes {
                    return Err(Error::Protocol(format!(
                        "SSE line exceeded limit of {} bytes",
                        self.max_line_bytes
                    )));
                }
                self.line(&bytes[..end], queue)?;
            } else {
                if self.pending.len().saturating_add(end) > self.max_line_bytes {
                    return Err(Error::Protocol(format!(
                        "SSE line exceeded limit of {} bytes",
                        self.max_line_bytes
                    )));
                }
                let mut line = std::mem::take(&mut self.pending);
                line.extend_from_slice(&bytes[..end]);
                self.line(&line, queue)?;
                line.clear();
                self.pending = line;
            }
            bytes = &bytes[end + 1..];
            if separator == b'\r' {
                self.skip_lf = bytes.is_empty();
                bytes = bytes.strip_prefix(b"\n").unwrap_or(bytes);
            }
        }
        if self.pending.len().saturating_add(bytes.len()) > self.max_line_bytes {
            return Err(Error::Protocol(format!(
                "SSE line exceeded limit of {} bytes",
                self.max_line_bytes
            )));
        }
        self.pending.extend_from_slice(bytes);
        Ok(())
    }

    pub fn finish(&mut self, queue: &mut VecDeque<SseEvent>) -> Result<()> {
        let tail = std::mem::take(&mut self.pending);
        if !tail.is_empty() {
            self.line(&tail, queue)?;
        }
        self.dispatch(queue);
        Ok(())
    }
}

pub struct SseReader<S> {
    inner: S,
    decoder: SseDecoder,
    events: VecDeque<SseEvent>,
    idle_timeout: Option<Duration>,
    done: bool,
}

impl<S, E> SseReader<S>
where
    S: Stream<Item = std::result::Result<Bytes, E>> + Unpin,
    Error: From<E>,
{
    pub fn new(stream: S) -> Self {
        Self::with_idle_timeout(stream, Some(DEFAULT_STREAM_IDLE_TIMEOUT))
    }

    pub fn with_idle_timeout(stream: S, idle_timeout: Option<Duration>) -> Self {
        Self {
            inner: stream,
            decoder: SseDecoder::default(),
            events: VecDeque::new(),
            idle_timeout,
            done: false,
        }
    }

    pub fn with_decoder_limits(
        mut self,
        max_line_bytes: usize,
        max_event_data_bytes: usize,
    ) -> Self {
        self.decoder = SseDecoder::with_limits(max_line_bytes, max_event_data_bytes);
        self
    }

    pub async fn next(&mut self, signal: &CancellationToken) -> Result<Option<SseEvent>> {
        loop {
            if let Some(event) = self.events.pop_front() {
                return Ok(Some(event));
            }
            if self.done {
                return Ok(None);
            }
            let chunk = abortable(signal, async {
                match self.idle_timeout.filter(|limit| !limit.is_zero()) {
                    Some(limit) => tokio::time::timeout(limit, self.inner.next())
                        .await
                        .map_err(|_| Error::Timeout(limit))?
                        .transpose()
                        .map_err(Error::from),
                    None => self.inner.next().await.transpose().map_err(Error::from),
                }
            })
            .await?;
            match chunk {
                Some(chunk) => self.decoder.push(&chunk, &mut self.events)?,
                None => {
                    self.done = true;
                    self.decoder.finish(&mut self.events)?;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use bytes::Bytes;
    use futures_util::Stream;
    use tokio_util::sync::CancellationToken;

    use super::{SseDecoder, SseReader};
    use crate::error::Error;

    #[test]
    fn decoder_interns_known_event_names_without_allocating() {
        let mut decoder = SseDecoder::default();
        let mut queue = VecDeque::new();
        decoder
            .push(
                b"event: content_block_delta\ndata: {\"a\":1}\n\nevent: custom_evt\ndata: ok\n\n",
                &mut queue,
            )
            .expect("valid SSE frames should decode");
        let first = queue.pop_front().expect("first event");
        let second = queue.pop_front().expect("second event");
        assert!(matches!(first.name, Cow::Borrowed("content_block_delta")));
        assert!(matches!(second.name, Cow::Owned(ref s) if s == "custom_evt"));
    }

    #[test]
    fn decoder_rejects_oversized_line_buffer() {
        let mut decoder = SseDecoder::with_limits(16, 64);
        let mut queue = VecDeque::new();
        let err = decoder
            .push(b"data: 0123456789abcdef", &mut queue)
            .expect_err("line exceeding 16 bytes must error");
        assert!(matches!(err, Error::Protocol(_)));
    }

    #[test]
    fn decoder_rejects_oversized_event_data() {
        let mut decoder = SseDecoder::with_limits(64, 10);
        let mut queue = VecDeque::new();
        let err = decoder
            .push(b"data: 123456\ndata: 78901\n\n", &mut queue)
            .expect_err("multi-line event data exceeding 10 bytes must error");
        assert!(matches!(err, Error::Protocol(_)));
    }

    struct PendingStream;
    impl Stream for PendingStream {
        type Item = std::result::Result<Bytes, std::io::Error>;

        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Pending
        }
    }

    #[tokio::test]
    async fn reader_returns_timeout_when_stream_stalls_past_idle_limit() {
        let mut reader =
            SseReader::with_idle_timeout(PendingStream, Some(Duration::from_millis(25)));
        let token = CancellationToken::new();
        let err = reader
            .next(&token)
            .await
            .expect_err("stalled stream must hit idle timeout");
        assert!(matches!(err, Error::Timeout(d) if d == Duration::from_millis(25)));
    }

    #[test]
    fn decoder_gives_same_events_across_chunk_sizes_and_line_endings() {
        for sep in ["\n", "\r\n", "\r"] {
            let text = [
                ": keepalive",
                "event: delta",
                "data: {\"a\":",
                "data:  \"e\"}",
                "",
                "data: tail",
            ]
            .join(sep);
            let bytes = text.as_bytes();
            for chunk_size in 1..=bytes.len() {
                let mut decoder = SseDecoder::default();
                let mut queue = VecDeque::new();
                for chunk in bytes.chunks(chunk_size) {
                    decoder.push(chunk, &mut queue).expect("push");
                }
                decoder.finish(&mut queue).expect("finish");
                let events: Vec<_> = queue.into_iter().collect();
                assert_eq!(events.len(), 2, "sep={sep:?} chunk_size={chunk_size}");
                assert_eq!(events[0].name, "delta");
                assert_eq!(events[0].data, "{\"a\":\n \"e\"}");
                assert_eq!(events[1].name, "");
                assert_eq!(events[1].data, "tail");
            }
        }
    }

    #[test]
    fn decoder_handles_split_crlf_empty_event_comment_only_and_large_line() {
        let mut decoder = SseDecoder::default();
        let mut queue = VecDeque::new();
        decoder.push(b"data: a\r", &mut queue).expect("push 1");
        assert!(queue.is_empty());
        decoder
            .push(b"\ndata: b\r\n\r\n", &mut queue)
            .expect("push 2");
        let ev = queue.pop_front().expect("split crlf event");
        assert_eq!(ev.data, "a\nb");

        decoder
            .push(b"event:\n\ndata: x\n\n: ping\n\ndata: y\n\n", &mut queue)
            .expect("push 3");
        assert_eq!(queue.pop_front().expect("ev1").name, "");
        assert_eq!(queue.pop_front().expect("ev2").data, "y");

        let large = format!("data: {}\n\n", "a".repeat(1024 * 1024));
        for chunk in large.as_bytes().chunks(4096) {
            decoder.push(chunk, &mut queue).expect("large chunk");
        }
        let ev = queue.pop_front().expect("large event");
        assert_eq!(ev.data.len(), 1024 * 1024);
    }
}
