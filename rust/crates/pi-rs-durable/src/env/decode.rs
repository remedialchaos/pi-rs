use encoding_rs::{CoderResult, Decoder, UTF_8};

/// Whether the input starts with the UTF-8 byte-order mark.
pub fn starts_with_bom(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\xef\xbb\xbf")
}

/// A streaming UTF-8 decoder that preserves U+FEFF, even at the start of a range.
pub struct RangeDecoder(Decoder);

pub fn range_decoder() -> RangeDecoder {
    RangeDecoder(UTF_8.new_decoder_without_bom_handling())
}

impl RangeDecoder {
    /// Decode one piece; `None` flushes an incomplete final character and resets the decoder.
    pub fn decode(&mut self, bytes: Option<&[u8]>) -> String {
        let mut text = String::new();
        self.decode_into(bytes, &mut text);
        text
    }

    fn decode_into(&mut self, bytes: Option<&[u8]>, text: &mut String) {
        let last = bytes.is_none();
        let mut bytes = bytes.unwrap_or_default();
        loop {
            text.reserve(bytes.len().clamp(4, 64 * 1024));
            let (result, read, _) = self.0.decode_to_string(bytes, text, last);
            if result == CoderResult::InputEmpty {
                break;
            }
            bytes = &bytes[read..];
        }
        if last {
            *self = range_decoder();
        }
    }
}

/// Decodes successive chunks like one file, omitting only its leading BOM.
pub struct StreamDecoder {
    decoder: RangeDecoder,
    started: bool,
}

impl Default for StreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamDecoder {
    pub fn new() -> Self {
        Self {
            decoder: range_decoder(),
            started: false,
        }
    }

    pub fn decode(&mut self, bytes: Option<&[u8]>) -> String {
        let mut text = String::new();
        self.decode_into(bytes, &mut text);
        text
    }

    pub(super) fn decode_into(&mut self, bytes: Option<&[u8]>, text: &mut String) {
        let start = text.len();
        self.decoder.decode_into(bytes, text);
        if !self.started && text.len() > start {
            self.started = true;
            if text[start..].starts_with('\u{feff}') {
                text.drain(start..start + 3);
            }
        }
    }
}
