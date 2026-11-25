use encoding_rs::{CoderResult, Decoder, UTF_8};

use super::{FileError, FileErrorCode, LineScan, MAX_SAFE_INTEGER};

/// Counts LF bytes while measuring one exclusive line range as decoded UTF-8, without buffering lines.
pub struct LineScanner {
    start_line: u64,
    end_line: Option<u64>,
    position: u64,
    newlines: u64,
    start: Option<u64>,
    end: Option<u64>,
    first_line_end: Option<u64>,
    last_line_start: u64,
    selected_bytes: u64,
    first_line_bytes: u64,
    decoder: Option<Decoder>,
}

impl LineScanner {
    pub fn new(start_line: u64, end_line: Option<u64>) -> Result<Self, FileError> {
        if start_line > MAX_SAFE_INTEGER
            || end_line.is_some_and(|end| end > MAX_SAFE_INTEGER || end <= start_line)
        {
            return Err(FileError::new(
                FileErrorCode::Invalid,
                "Invalid line range",
                None,
            ));
        }
        Ok(Self {
            start_line,
            end_line,
            position: 0,
            newlines: 0,
            start: (start_line == 0).then_some(0),
            end: None,
            first_line_end: None,
            last_line_start: 0,
            selected_bytes: 0,
            first_line_bytes: 0,
            decoder: (start_line == 0).then(|| UTF_8.new_decoder_with_bom_removal()),
        })
    }

    fn feed(&mut self, mut bytes: &[u8], last: bool) {
        let Some(decoder) = &mut self.decoder else {
            return;
        };
        let mut buffer = [0; 4096];
        loop {
            let (result, read, written, _) = decoder.decode_to_utf8(bytes, &mut buffer, last);
            self.selected_bytes += written as u64;
            if result == CoderResult::InputEmpty {
                break;
            }
            bytes = &bytes[read..];
        }
    }

    pub fn push(&mut self, chunk: &[u8]) {
        let mut from = 0;
        for (i, byte) in chunk.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            let position = self.position + i as u64;
            if self.newlines == self.start_line {
                // LF is ASCII: an incomplete sequence immediately before it is replaced, as at EOF.
                self.feed(&chunk[from..i], true);
                self.first_line_end = Some(position);
                self.first_line_bytes = self.selected_bytes;
                self.decoder = Some(UTF_8.new_decoder_without_bom_handling());
                from = i;
            }
            self.newlines += 1;
            if self.end_line == Some(self.newlines) {
                self.feed(&chunk[from..i], true);
                self.end = Some(position);
                self.decoder = None;
            }
            if self.newlines == self.start_line {
                self.start = Some(position + 1);
                self.decoder = Some(UTF_8.new_decoder_without_bom_handling());
                from = i + 1;
            }
            if self.decoder.is_some() {
                self.last_line_start = position + 1;
            }
        }
        self.feed(&chunk[from..], false);
        self.position += chunk.len() as u64;
    }

    pub fn finish(mut self) -> LineScan {
        self.feed(&[], true);
        let size = self.position;
        LineScan {
            newlines: self.newlines,
            start: self.start.unwrap_or(size),
            end: self.end.unwrap_or(size),
            first_line_end: self.first_line_end.unwrap_or(size),
            last_line_start: self.start.map_or(size, |_| self.last_line_start),
            selected_bytes: self.selected_bytes,
            first_line_bytes: self
                .first_line_end
                .map_or(self.selected_bytes, |_| self.first_line_bytes),
        }
    }
}
