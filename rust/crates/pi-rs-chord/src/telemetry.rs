//! Crate-owned recording and fallible ingest/sender hooks. Shared adapter installation is deliberately external.
mod facet;
mod recorder;
mod sender;
pub use facet::{
    IngestFacet, IngestOptions, IngestRegistry, PageIngest, Receipt, WorkerIngest,
    page_batch_schema, worker_batch_schema,
};
pub use recorder::{Recorder, RecordingContext, RecordingSpan, SpanIdentity};
pub use sender::{SendKind, SqliteTelemetry, TelemetrySender, TelemetrySink};
