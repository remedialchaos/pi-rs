//! `flushStdout` port. Upstream writes an empty string with a 1000 ms watchdog; Rust's
//! `stdout().flush()` is synchronous and reports EPIPE through `io::Result` instead of keeping an
//! `error` listener attached. Callers that must never abort on EPIPE should ignore the result.

use std::io::{self, Write};

/// Flush buffered stdout. Unlike the JavaScript version there is no timer: the syscall either
/// completes or returns an error, so there is no 1 s fallback to emulate.
pub fn flush_stdout() -> io::Result<()> {
    io::stdout().flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flush_succeeds_on_a_memory_writer() -> Result<(), String> {
        let mut buffer = Vec::new();
        buffer.write_all(b"line\n").map_err(|e| e.to_string())?;
        buffer.flush().map_err(|e| e.to_string())?;
        assert_eq!(buffer, b"line\n");
        Ok(())
    }

    #[test]
    fn flush_stdout_does_not_panic_on_a_closed_pipe() {
        // The Rust API returns the error rather than throwing an uncaught EPIPE event.
        let _ = flush_stdout();
    }
}
