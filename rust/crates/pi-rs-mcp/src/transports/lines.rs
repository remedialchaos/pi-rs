//! Reads a program's output as MCP's stdio framing (`packages/mcp/src/transports/lines.ts`):
//! newline-delimited JSON-RPC messages on stdout, and a bounded tail of stderr for diagnostics.

use std::sync::Arc;

use crate::protocol::{JsonRpcMessage, McpErrorEvent, parse_json, parse_json_rpc_message};
use crate::transports::DEFAULT_MAX_MESSAGE_BYTES;

/// Every stderr chunk, decoded as UTF-8.
pub type OnStderr = Arc<dyn Fn(&str) + Send + Sync>;
/// Every complete stdout message.
pub type OnMessage = Arc<dyn Fn(&JsonRpcMessage) + Send + Sync>;
/// Parse failures and size violations, by message.
pub type OnError = Arc<dyn Fn(McpErrorEvent) + Send + Sync>;

/// Options of [`JsonRpcLineReader`].
#[derive(Clone)]
pub struct JsonRpcLineReaderOptions {
    pub max_message_bytes: Option<u64>,
    pub max_stderr_bytes: Option<u64>,
    pub on_stderr: Option<OnStderr>,
    pub on_message: OnMessage,
    pub on_error: OnError,
}

/// The default size of the stderr tail.
const DEFAULT_MAX_STDERR_BYTES: u64 = 64 * 1024;

/// Reads a program's output as MCP's stdio framing: newline-delimited JSON-RPC messages on
/// stdout, and a bounded tail of stderr for diagnostics.
pub struct JsonRpcLineReader {
    options: JsonRpcLineReaderOptions,
    /// The bytes of the incomplete message received so far.
    stdout_chunks: Vec<Vec<u8>>,
    stdout_pending_bytes: usize,
    stderr_buffer: Vec<u8>,
}

impl JsonRpcLineReader {
    pub fn new(options: JsonRpcLineReaderOptions) -> Self {
        Self {
            options,
            stdout_chunks: Vec::new(),
            stdout_pending_bytes: 0,
            stderr_buffer: Vec::new(),
        }
    }

    /// The last `max_stderr_bytes` of stderr.
    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.stderr_buffer).into_owned()
    }

    /// Takes the next stdout bytes and reports every complete message, or the error parsing it.
    /// Bytes of an incomplete message are kept as chunks and joined once when its newline arrives,
    /// so a large message that arrives in many chunks is copied and scanned only once.
    pub fn read_stdout(&mut self, chunk: &[u8]) {
        let max_message_bytes = self
            .options
            .max_message_bytes
            .unwrap_or(DEFAULT_MAX_MESSAGE_BYTES);
        let mut start = 0;
        loop {
            let Some(offset) = chunk[start..].iter().position(|byte| *byte == 0x0a) else {
                if start < chunk.len() {
                    // Copied, since the caller may reuse the chunk's memory.
                    self.stdout_chunks.push(chunk[start..].to_vec());
                    self.stdout_pending_bytes += chunk.len() - start;
                }
                if self.stdout_pending_bytes > max_message_bytes as usize {
                    self.stdout_chunks.clear();
                    self.stdout_pending_bytes = 0;
                    (self.options.on_error)(McpErrorEvent::message(format!(
                        "MCP stdio message exceeds {max_message_bytes} bytes"
                    )));
                }
                return;
            };
            let newline = start + offset;
            let line: Vec<u8> = if self.stdout_chunks.is_empty() {
                chunk[start..newline].to_vec()
            } else {
                self.stdout_chunks.push(chunk[start..newline].to_vec());
                let joined = self.stdout_chunks.concat();
                self.stdout_chunks.clear();
                self.stdout_pending_bytes = 0;
                joined
            };
            start = newline + 1;
            if line.len() > max_message_bytes as usize {
                (self.options.on_error)(McpErrorEvent::message(format!(
                    "MCP stdio message exceeds {max_message_bytes} bytes"
                )));
                continue;
            }
            let text = String::from_utf8_lossy(&line);
            let text = text.strip_suffix('\r').unwrap_or(&text);
            if text.trim_matches(is_ecmascript_space).is_empty() {
                continue;
            }
            match parse_json(text) {
                Ok(value) => match parse_json_rpc_message(&value) {
                    Ok(message) => (self.options.on_message)(&message),
                    Err(error) => (self.options.on_error)(McpErrorEvent::from_mcp(&error)),
                },
                Err(error) => (self.options.on_error)(McpErrorEvent::message(error)),
            }
        }
    }

    /// Takes the next stderr bytes into the tail and passes them to `on_stderr`.
    pub fn read_stderr(&mut self, chunk: &[u8]) {
        let max_stderr_bytes = self
            .options
            .max_stderr_bytes
            .unwrap_or(DEFAULT_MAX_STDERR_BYTES) as usize;
        self.stderr_buffer.extend_from_slice(chunk);
        if self.stderr_buffer.len() > max_stderr_bytes {
            let overflow = self.stderr_buffer.len() - max_stderr_bytes;
            self.stderr_buffer.drain(..overflow);
        }
        if let Some(on_stderr) = &self.options.on_stderr {
            on_stderr(&String::from_utf8_lossy(chunk));
        }
    }

    /// Ends stdout: a message without its newline is reported as an error and dropped.
    pub fn end(&mut self) {
        let pending = self.stdout_chunks.concat();
        if !pending.is_empty()
            && !String::from_utf8_lossy(&pending)
                .trim_matches(is_ecmascript_space)
                .is_empty()
        {
            (self.options.on_error)(McpErrorEvent::message(
                "MCP stdio server closed with an incomplete JSON-RPC message",
            ));
        }
        self.stdout_chunks.clear();
        self.stdout_pending_bytes = 0;
    }
}

/// Whether a character is one `String.prototype.trim()` removes.
fn is_ecmascript_space(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}
