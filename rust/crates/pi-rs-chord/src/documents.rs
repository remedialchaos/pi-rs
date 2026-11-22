//! Versioned document slots, sealed attempt handles and atomic store-backed publication.
mod definition;
mod line;
mod persistence;
mod runtime;
pub use definition::{Definition, KeyedBy};
pub use line::{Line, LineLease, SealToken};
pub use persistence::{DocumentStore, Stored};
pub use runtime::{Attempt, DocumentKey, Documents, Draft, Handle, Pin, Staged, Transaction};
