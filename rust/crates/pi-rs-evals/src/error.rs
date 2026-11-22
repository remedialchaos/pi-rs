//! Shared error type for the evaluation runner.

use std::fmt::{Display, Formatter};

/// Errors produced by the evaluation planner, reporter, docker runner, and CLI.
#[derive(Debug)]
pub enum EvalError {
    /// An explicit validation or runtime failure with a message matching upstream.
    Message(String),
    /// Filesystem or process failure.
    Io(std::io::Error),
    /// JSON parse or serialize failure.
    Json(serde_json::Error),
}

impl EvalError {
    /// Construct an error carrying an explicit message.
    pub fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}

impl Display for EvalError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message(message) => formatter.write_str(message),
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Json(error) => write!(formatter, "JSON error: {error}"),
        }
    }
}

impl std::error::Error for EvalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Message(_) => None,
        }
    }
}

impl From<std::io::Error> for EvalError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for EvalError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// Result alias used across the crate.
pub type EvalResult<T> = Result<T, EvalError>;

#[cfg(test)]
mod tests {
    use super::EvalError;

    #[test]
    fn message_display_is_exact() {
        let error = EvalError::message("Documentation variant mismatch.");
        assert_eq!(error.to_string(), "Documentation variant mismatch.");
    }
}
