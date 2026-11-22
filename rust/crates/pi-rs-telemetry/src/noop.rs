//! Shared inert capabilities. No-op handles do not retain caller metadata or carriers.
use crate::{Context, Metrics, memory::Propagation};
pub const NOOP_TELEMETRY_CONTEXT: Context = Context::noop();
pub const NOOP_TELEMETRY_METRICS: Metrics = Metrics::noop();
pub const NOOP_TELEMETRY_PROPAGATION: Propagation = Propagation {
    root: Context::noop(),
};
