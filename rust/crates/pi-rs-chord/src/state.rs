//! Host-local atomic publication, lazy derives and identity-based native values.
mod batch;
mod native;
mod shared;
mod source;
pub use batch::{Batch, BatchError};
pub use native::{NativeObservation, NativeSource};
pub use shared::{MapShared, map_shared};
pub use source::{Constant, Source, Subscription, constant};
