//! Chord's portable data, persistence and protocol foundation.
#![forbid(unsafe_code)]

pub mod context;
pub mod contracts;
pub mod data;
pub mod delta;
pub mod documents;
pub mod json;
pub mod link;
pub mod output;
pub mod sqlite;
pub mod state;
pub mod store;
pub mod sync;
pub mod telemetry;
pub mod telemetry_ingest;
pub mod telemetry_sqlite;
pub mod watch;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Delta(#[from] delta::DeltaError),
    #[error(transparent)]
    Chord(Box<data::ChordError>),
    #[error("{0}")]
    Invalid(String),
    #[error("store diverged: {0}")]
    Diverged(String),
    #[error("SQLite database is closed")]
    Closed,
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("SQLite transaction failed ({operation}) and rollback failed ({rollback})")]
    Rollback {
        operation: Box<Error>,
        rollback: rusqlite::Error,
    },
}
impl From<data::ChordError> for Error {
    fn from(error: data::ChordError) -> Self {
        Self::Chord(Box::new(error))
    }
}
pub type Result<T> = std::result::Result<T, Error>;
