use std::io;

use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

/// Serialize one LF-delimited record. Unicode line separators in the payload are not delimiters.
pub fn serialize_json_line(value: &impl Serialize) -> serde_json::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Reads LF-delimited records, including a final unterminated record. Reuses its byte buffer.
pub struct JsonlReader<R> {
    reader: BufReader<R>,
    bytes: Vec<u8>,
    pending: bool,
}

impl<R: AsyncRead + Unpin> JsonlReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(reader),
            bytes: Vec::new(),
            pending: false,
        }
    }

    /// Safe to resume after cancellation in `select!`, even partway through a record.
    pub async fn next_line(&mut self) -> io::Result<Option<std::borrow::Cow<'_, str>>> {
        if !self.pending {
            self.bytes.clear();
            self.pending = true;
        }
        self.reader.read_until(b'\n', &mut self.bytes).await?;
        self.pending = false;
        if self.bytes.is_empty() {
            return Ok(None);
        }
        let line = self.bytes.strip_suffix(b"\n").unwrap_or(&self.bytes);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        Ok(Some(String::from_utf8_lossy(line)))
    }
}
