use std::time::Duration;

use reqwest::{StatusCode, header::HeaderMap};
use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    #[error("Request aborted")]
    Aborted,
    #[error("Unknown provider: {0}")]
    Provider(String),
    #[error("{0}")]
    Auth(String),
    #[error("{0}")]
    OAuth(String),
    #[error("{0}")]
    ModelSource(String),
    #[error("{0}")]
    Protocol(String),
    #[error("{message}")]
    Http {
        status: StatusCode,
        headers: Box<HeaderMap>,
        message: String,
    },
    #[error("Request timed out after {0:?}")]
    Timeout(Duration),
    #[error(transparent)]
    Transport(#[from] reqwest::Error),
    #[error(transparent)]
    Url(#[from] url::ParseError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("Assistant event stream ended without a result")]
    MissingTerminalEvent,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
