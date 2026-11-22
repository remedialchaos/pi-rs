//! Portable conformance cases, independent of any test framework or executor.
pub mod conformance;
pub mod types;
pub use conformance::create_telemetry_adapter_conformance;
pub use types::*;
