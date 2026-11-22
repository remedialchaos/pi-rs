use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Request(String),
    #[error("{0}")]
    Model(String),
    #[error("{0}")]
    Config(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Pool(#[from] rayon::ThreadPoolBuildError),
}

pub type Result<T> = std::result::Result<T, Error>;
