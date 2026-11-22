use std::collections::VecDeque;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::http::abortable;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SseEvent {
    pub name: String,
    pub data: String,
}

#[derive(Default)]
pub struct SseDecoder {
    pending: Vec<u8>,
    data: String,
    name: String,
    has_data: bool,
    skip_lf: bool,
    saw_first_line: bool,
}
impl SseDecoder {
    fn dispatch(&mut self, queue: &mut VecDeque<SseEvent>) {
        if self.has_data {
            self.has_data = false;
            queue.push_back(SseEvent {
                name: std::mem::take(&mut self.name),
                data: std::mem::take(&mut self.data),
            });
        } else {
            self.name.clear();
        }
    }
    fn line(&mut self, line: &[u8], queue: &mut VecDeque<SseEvent>) {
        let line = if self.saw_first_line {
            line
        } else {
            self.saw_first_line = true;
            line.strip_prefix(b"\xef\xbb\xbf").unwrap_or(line)
        };
        if line.is_empty() {
            self.dispatch(queue);
            return;
        }
        let split = line.iter().position(|b| *b == b':').unwrap_or(line.len());
        let value = line.get(split + 1..).unwrap_or_default();
        let value = value.strip_prefix(b" ").unwrap_or(value);
        match &line[..split] {
            b"event" => {
                self.name.clear();
                self.name.push_str(&String::from_utf8_lossy(value));
            }
            b"data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.has_data = true;
                self.data.push_str(&String::from_utf8_lossy(value));
            }
            _ => {}
        }
    }
    pub fn push(&mut self, mut bytes: &[u8], queue: &mut VecDeque<SseEvent>) {
        if self.skip_lf && !bytes.is_empty() {
            self.skip_lf = false;
            bytes = bytes.strip_prefix(b"\n").unwrap_or(bytes);
        }
        while let Some(end) = bytes.iter().position(|b| matches!(b, b'\r' | b'\n')) {
            let separator = bytes[end];
            if self.pending.is_empty() {
                self.line(&bytes[..end], queue);
            } else {
                let mut line = std::mem::take(&mut self.pending);
                line.extend_from_slice(&bytes[..end]);
                self.line(&line, queue);
                line.clear();
                self.pending = line;
            }
            bytes = &bytes[end + 1..];
            if separator == b'\r' {
                self.skip_lf = bytes.is_empty();
                bytes = bytes.strip_prefix(b"\n").unwrap_or(bytes);
            }
        }
        self.pending.extend_from_slice(bytes);
    }
    pub fn finish(&mut self, queue: &mut VecDeque<SseEvent>) {
        let tail = std::mem::take(&mut self.pending);
        if !tail.is_empty() {
            self.line(&tail, queue);
        }
        self.dispatch(queue);
    }
}

pub struct SseReader<S> {
    inner: S,
    decoder: SseDecoder,
    events: VecDeque<SseEvent>,
    done: bool,
}
impl<S: Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Unpin> SseReader<S> {
    pub fn new(stream: S) -> Self {
        Self {
            inner: stream,
            decoder: SseDecoder::default(),
            events: VecDeque::new(),
            done: false,
        }
    }
    pub async fn next(&mut self, signal: &CancellationToken) -> Result<Option<SseEvent>> {
        loop {
            if let Some(event) = self.events.pop_front() {
                return Ok(Some(event));
            }
            if self.done {
                return Ok(None);
            }
            match abortable(signal, async { Ok(self.inner.next().await.transpose()?) }).await? {
                Some(chunk) => self.decoder.push(&chunk, &mut self.events),
                None => {
                    self.done = true;
                    self.decoder.finish(&mut self.events);
                }
            }
        }
    }
}
